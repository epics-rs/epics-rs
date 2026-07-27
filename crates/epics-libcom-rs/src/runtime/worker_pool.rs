//! A bounded set of persistent threads that a connection **borrows** rather
//! than creates, closing the server-side per-connection thread leak.
//!
//! # The defect this closes
//!
//! Every `std::thread` *creation* leaves 176–179 B behind permanently on
//! RTEMS 6 — the thread's TLS key is freed before its destructor runs, so the
//! value block is never reclaimed. The cost is per *creation*, not per live
//! thread, so a server that spawns a thread per accepted connection leaks
//! without a ceiling: a client that connects and disconnects in a loop drains
//! the target's fixed heap for as long as the IOC runs.
//!
//! [`DialPool`](super::blocking_io::DialPool) closed the client *dial* path by
//! this argument. This is the same argument on the *serve* side, and the two
//! are separate primitives on purpose (`doc/rtems-connection-worker-pool-design.md`
//! §3): a dial borrows one worker for a short job and *queues* over capacity; a
//! connection borrows a **set** of workers for its whole life and is *refused*
//! over capacity.
//!
//! # The unit of borrow is a set, not a worker
//!
//! A PVA connection needs **three** threads *together* — the connection thread
//! plus a reader pump plus a writer pump. If it could take two and block for the
//! third, a server at capacity would deadlock: every connection holding two,
//! each waiting on one nobody will free. So [`WorkerPool::acquire`] hands out a
//! whole [`Worker`] array or nothing, and there is no API that borrows one
//! worker on its own. The roster is heterogeneous within a set (the PVA set is
//! one `Big` stack and two `Small`), which is exactly why a *set* is the unit
//! and not N draws from N per-class pools — drawing separately would reopen the
//! partial-borrow deadlock.
//!
//! # It does not raise the connection ceiling
//!
//! A pooled connection occupies the same three stacks while it is live, because
//! it is the same three threads doing the same work; the ceiling is
//! per-connection *memory* (1,589,554 B measured per PVA connection on
//! `armv7-rtems-eabihf`), not thread-creation residue. What the pool removes is
//! the *residue of the creation*. Its other job is to be the single owner of
//! connection admission — the one place that can refuse with `EAGAIN`.
//!
//! # The bound is memory, not just a count
//!
//! A count bound cannot know when the target is out of thread memory: on
//! `x86_64-wrs-vxworks` the CA pool's count bound (141, derived from the
//! descriptor budget) was never reached, because the process hit a reserved
//! address-space ceiling at 46 concurrent clients first — and what happened
//! there was not a refusal. `pthread_create` began to fail, then a `std` mutex
//! lock returned `EINVAL` and killed a worker, then an allocation of 64 bytes
//! failed and took the whole RTP down with signal 6. A bound that is reached
//! *after* the target has run out is not an admission gate.
//!
//! So every set's memory is reserved from one **process-wide** budget before a
//! thread is created ([`try_reserve`], [`POOL_RESERVATION_ENV`]), and a set
//! that does not fit is refused with [`AcquireError::OutOfReservation`] while
//! the target still has the memory to deliver the refusal. Process-wide because
//! the resource is: an IOC runs several pools, and three pools each inside
//! their own bound can still walk the process past the ceiling together. The
//! count bounds stay exactly what they were — `capacity` is a descriptor bound
//! for the CA server and an operator's `max_connections` for the PVA server —
//! because those are different resources and folding them into one number is
//! what makes a bound unable to say which one ran out.
//!
//! # Accounting: busy is what is counted, and a set idles through one gate
//!
//! A set returns to the idle pool when **both** its [`SetLease`] has dropped
//! *and* every job dispatched on it has returned. `running` is incremented only
//! by [`Worker::run`]/[`Worker::run_detached`] (the actor that really
//! dispatched) and decremented only by the worker loop after the job's closure
//! has fully returned and been dropped (the actor that really finished). No side
//! path pokes it. Every transition — lease drop, job completion — locks the
//! set's own state once, mutates, and checks the idle condition behind a
//! `parked` flag so a double push is unrepresentable; only then, and never while
//! holding the set lock, does it touch the pool lock to push the set back.
//!
//! # A worker that dies retires its set
//!
//! That accounting is exact only while every dispatched job comes back, and a
//! worker *thread* can die where the job's `catch_unwind` does not reach — on
//! target it did, at the memory wall, in a `std` mutex that returned `EINVAL`.
//! So the set's slot is released by the thread's destructor ([`WorkerExit`]),
//! not by a code path that a panic can skip: any exit that was not asked for
//! marks the set dead, stops its siblings, and gives the slot back to `created`
//! when the last of its threads is gone. A dead set is never pooled again,
//! because a set one thread short cannot serve a connection — and a dispatch
//! that lands on a worker already gone is reported as *not run*, never as a
//! clean completion.

use std::collections::VecDeque;
use std::io;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, Sender, SyncSender, channel, sync_channel};
use std::sync::{Arc, LazyLock, Mutex};
use std::thread::{self, JoinHandle};

use crate::runtime::log::{ErrlogSevEnum, errlog_sev_printf};
use crate::runtime::task::{InheritedRuntime, StackSizeClass, ThreadPriority, enter_ioc_thread};

/// One member of a worker set: how its thread is named, sized and banded.
#[derive(Clone, Copy, Debug)]
pub struct WorkerRole {
    /// OS thread-name stem; the thread is `"{pool_prefix}-{suffix} {set}"`.
    /// Keep it short — RTEMS truncates thread names at 16 bytes.
    pub suffix: &'static str,
    /// The stack this role's work needs. The set is heterogeneous: the PVA
    /// connection thread is `Big`, its two pumps are `Small`.
    pub stack: StackSizeClass,
    /// The EPICS band this role's thread takes, for its whole life.
    pub priority: ThreadPriority,
}

/// The work handed to one worker for one job, captured on the submitting thread.
enum Assignment {
    /// A job whose panic result is handed to a joiner ([`Job::join`]).
    Joinable {
        body: Box<dyn FnOnce() + Send + 'static>,
        ambient: InheritedRuntime,
        done: SyncSender<thread::Result<()>>,
    },
    /// A job nobody joins — the worker itself announces a panic.
    Detached {
        body: Box<dyn FnOnce() + Send + 'static>,
        ambient: InheritedRuntime,
        label: String,
    },
    /// Leave the worker loop. Sent once per worker at pool teardown.
    Stop,
}

/// Everything a set mutates while it is leased, under one lock.
struct SetState {
    /// A borrower holds the [`SetLease`].
    leased: bool,
    /// Jobs dispatched on this set that have not yet returned.
    running: usize,
    /// The set is currently sitting in the pool's idle deque. Guards against a
    /// second push: the lease drop and the last job completion can race, and
    /// exactly one of them must move the set to idle.
    parked: bool,
    /// Threads of this set that have not yet exited. Reaches zero exactly once,
    /// on the last thread's exit, which is what releases the set's slot.
    live_workers: usize,
}

impl SetState {
    /// Mutate under the lock, then answer *did this transition free the set?* —
    /// true at most once per lease, because `parked` latches.
    ///
    /// Whether a freed set may be *re-pooled* is not decided here: a retired set
    /// still transitions to free, and [`free_if_idle`] is the single gate that
    /// drops it instead of pushing it.
    fn became_free(&mut self) -> bool {
        if !self.leased && self.running == 0 && !self.parked {
            self.parked = true;
            true
        } else {
            false
        }
    }
}

/// A live set: its per-slot job senders and its shared accounting.
struct SetHandle {
    /// This set's creation index — its thread-name suffix, and how a retirement
    /// names itself on the console.
    index: usize,
    /// One sender per role, cloned into the [`Worker`]s handed out at lease.
    senders: Vec<Sender<Assignment>>,
    /// One of this set's threads has exited. A dead set never idles again and
    /// never leases again: its survivors are stopped and its slot goes back to
    /// the bound once they are all gone.
    ///
    /// An atomic rather than a [`SetState`] field because [`free_if_idle`] must
    /// read it while holding the *pool* lock, and the two locks have a fixed
    /// order — the pool lock is never taken inside a set lock — so consulting
    /// `state` from there would be the deadlock the order exists to prevent.
    dead: AtomicBool,
    state: Mutex<SetState>,
}

/// Everything the pool mutates, under one lock. Never taken while a set lock is
/// held.
struct Registry {
    /// Sets ready to be leased.
    idle: VecDeque<Arc<SetHandle>>,
    /// Every set ever created, leased or idle. Kept so teardown can reach every
    /// worker's sender regardless of whether its set is currently borrowed.
    all: Vec<Arc<SetHandle>>,
    /// Sets created, ever (`== all.len()` once a grow settles). Reserved
    /// *before* the threads are spawned and only decremented if the spawn fails,
    /// so it is the true bound on creations — the number the per-connection
    /// shape grew without limit.
    created: usize,
    /// Every worker thread, for the join at teardown.
    joins: Vec<JoinHandle<()>>,
    /// Set once teardown has begun; a set freed after this is not re-pooled.
    stopping: bool,
}

struct PoolInner {
    /// One entry per role; its length is the set size `N`.
    roster: Box<[WorkerRole]>,
    /// Thread-name stem shared by every worker.
    name_prefix: &'static str,
    /// The most sets that may ever exist — connection admission's hard bound.
    capacity: usize,
    /// What one whole set reserves: the sum over the roster of
    /// [`thread_reservation`]. Fixed by the roster, so admission needs no
    /// per-set arithmetic.
    set_reservation: usize,
    /// The account this pool reserves from and releases to. One and the same
    /// for every production pool, so a release cannot land anywhere but where
    /// its reservation came from.
    reservation: &'static Reservation,
    /// The object-arena gate — [`materialise_set_mutex`] in every production
    /// pool. A field so a test can make the target refuse, which is the one
    /// thing a host cannot be made to do.
    materialise: fn(&Mutex<SetState>) -> bool,
    reg: Mutex<Registry>,
}

impl PoolInner {
    fn lock(&self) -> std::sync::MutexGuard<'_, Registry> {
        self.reg.lock().unwrap_or_else(|e| e.into_inner())
    }
}

fn lock_set(set: &SetHandle) -> std::sync::MutexGuard<'_, SetState> {
    set.state.lock().unwrap_or_else(|e| e.into_inner())
}

/// Push a set back to idle if a transition just freed it. Called from both the
/// lease drop and the worker loop; the set lock is released before the pool lock
/// is taken.
fn free_if_idle(inner: &Arc<PoolInner>, set: &Arc<SetHandle>, freed: bool) {
    if !freed {
        return;
    }
    let mut reg = inner.lock();
    if reg.stopping {
        // Teardown owns this set now; leaving it out of `idle` is what makes a
        // freed set stay freed.
        return;
    }
    if set.dead.load(Ordering::SeqCst) {
        // A retiring set must not be pooled. The read is under the pool lock and
        // so is `WorkerExit`'s removal, so whichever runs second sees the other:
        // either this push happens first and the removal takes it back out, or
        // the death is already visible here and no push happens at all.
        return;
    }
    reg.idle.push_back(set.clone());
}

// ---------------------------------------------------------------------------
// The lease
// ---------------------------------------------------------------------------

/// Proof that a set is borrowed. Its `Drop` is half of the return condition:
/// the set cannot re-idle until this is gone *and* every job has finished.
///
/// The borrower holds it for the connection's whole life — the PVA server moves
/// it into the connection job on the set's own worker, and a pooled client holds
/// it through the two returned byte adapters — so the set stays out of the idle
/// pool for exactly as long as the connection lasts.
pub struct SetLease {
    inner: Arc<PoolInner>,
    set: Arc<SetHandle>,
}

impl Drop for SetLease {
    fn drop(&mut self) {
        let freed = {
            let mut st = lock_set(&self.set);
            st.leased = false;
            st.became_free()
        };
        free_if_idle(&self.inner, &self.set, freed);
    }
}

// ---------------------------------------------------------------------------
// A single leased worker
// ---------------------------------------------------------------------------

/// One thread of a leased set, able to run exactly one job.
///
/// `run`/`run_detached` consume the worker, so a role cannot be double-booked;
/// and `acquire` is the only source of a `Worker`, so a connection cannot use a
/// worker it did not lease. Both facts hold by type, not by review.
pub struct Worker {
    inner: Arc<PoolInner>,
    set: Arc<SetHandle>,
    tx: Sender<Assignment>,
}

/// What a [`Job::join`] reports for a job that was never dispatched: the
/// worker's receiver was already gone, so no body ever ran.
const NEVER_DISPATCHED: &str =
    "worker pool: the job was never dispatched — its worker thread had already exited";

impl Worker {
    /// Count this dispatch against the set before the worker can finish it.
    fn charge(&self) {
        lock_set(&self.set).running += 1;
    }

    /// Dispatch a job whose completion — and any panic — is observed by the
    /// returned [`Job`].
    pub fn run<F>(self, body: F) -> Job
    where
        F: FnOnce() + Send + 'static,
    {
        let (done, done_rx) = sync_channel(1);
        self.charge();
        if self
            .tx
            .send(Assignment::Joinable {
                body: Box::new(body),
                ambient: InheritedRuntime::capture(),
                done,
            })
            .is_err()
        {
            // The worker died before this dispatch. Give back the charge the
            // worker will now never settle, and tell the joiner the truth: the
            // body did not run.
            finish_job(&self.inner, &self.set);
            // The joiner learns *that* the job did not run; only here is the
            // *reason* known, and a joiner that reports its own loss ("the pump
            // panicked") would otherwise name the wrong cause.
            errlog_sev_printf(
                ErrlogSevEnum::Major,
                &format!(
                    "{} worker pool: set {} took no job — {NEVER_DISPATCHED}.",
                    self.inner.name_prefix, self.set.index
                ),
            );
            return Job { done: None };
        }
        Job {
            done: Some(done_rx),
        }
    }

    /// Dispatch a job nobody will join. The worker announces a panic through
    /// `errlog` under `label`; a clean return is silent.
    pub fn run_detached<F>(self, label: String, body: F)
    where
        F: FnOnce() + Send + 'static,
    {
        self.charge();
        if self
            .tx
            .send(Assignment::Detached {
                body: Box::new(body),
                ambient: InheritedRuntime::capture(),
                label: label.clone(),
            })
            .is_err()
        {
            finish_job(&self.inner, &self.set);
            // Nobody joins a detached job, so `errlog` is the only place this
            // can be told; silence here is the loss this pool exists to stop.
            errlog_sev_printf(
                ErrlogSevEnum::Major,
                &format!("{label}: {NEVER_DISPATCHED}. This connection is being torn down."),
            );
        }
    }
}

/// A handle to a running job, joined on the borrower's teardown path.
pub struct Job {
    /// `None` when the dispatch itself failed: there is no worker to hear from.
    done: Option<Receiver<thread::Result<()>>>,
}

impl Job {
    /// Block until the job returns, yielding whether it panicked.
    ///
    /// A dropped sender (the worker gone at teardown after the job was taken)
    /// reads as a clean completion: there is no unwind to report and nothing to
    /// tear down twice. A job that was never dispatched at all is *not* clean —
    /// it reports [`NEVER_DISPATCHED`], because a borrower that is told its
    /// body succeeded when it never ran is the same silent loss as a dropped
    /// panic payload.
    pub fn join(self) -> thread::Result<()> {
        match self.done {
            Some(done) => done.recv().unwrap_or(Ok(())),
            None => Err(Box::new(NEVER_DISPATCHED)),
        }
    }
}

// ---------------------------------------------------------------------------
// The pool
// ---------------------------------------------------------------------------

/// Announce a connection job that unwound, the way the byte pumps announce a
/// lost pump: through `errlog`, which reaches the console whatever the log
/// configuration is — including an RTEMS console with the in-tree subscriber.
fn announce_panic(label: &str) {
    errlog_sev_printf(
        ErrlogSevEnum::Major,
        &format!(
            "{label}: the connection thread panicked; this connection is being \
             torn down. Other connections are unaffected."
        ),
    );
}

/// Announce a worker thread that left without being asked to. One record per
/// set, on the first death, naming what the pool lost.
fn announce_worker_death(prefix: &str, index: usize, roles: usize) {
    errlog_sev_printf(
        ErrlogSevEnum::Major,
        &format!(
            "{prefix} worker pool: a thread of set {index} exited unexpectedly. \
             The set's {roles} threads are being retired and its slot returned; \
             other connections are unaffected."
        ),
    );
}

/// The one exit path of a worker thread.
///
/// # The defect this closes
///
/// `catch_unwind` around the job body does not make the *thread* unwind-proof:
/// dropping the panic payload after the joiner is gone, and the two mutex takes
/// in the return path, all sit outside it. On VxWorks 7 the second of those is
/// not hypothetical — a `std` mutex lock at the memory wall returns `EINVAL`
/// and `std` panics, killing the worker between `catch_unwind` and
/// `finish_job`. The set then had `running == 1` forever with no thread to
/// settle it: never idle, never re-leased, its slot held against the pool's
/// bound for the life of the process (`BUSY=2 SETS=50 WORKERS=100 CONNS=0`
/// measured on target).
///
/// So the accounting hangs off the thread's *destructor*, not off a code path:
/// however the thread ends — `Stop`, a closed channel, or an unwind anywhere
/// including the prologue — this runs. A death retires the whole set, because a
/// set one thread short can never serve a connection again.
struct WorkerExit {
    inner: Arc<PoolInner>,
    set: Arc<SetHandle>,
    /// This thread's share of its set's reservation, given back here — the one
    /// place that runs for a thread that exists and never for one that does not.
    reserved: usize,
    /// Set true only where the worker returns normally. Left false on every
    /// unwind, which is what tells the two apart in `Drop`.
    clean: bool,
}

impl Drop for WorkerExit {
    fn drop(&mut self) {
        // Unconditional and first: the thread's memory goes back to the process
        // budget however the thread ended. Every other decision below is about
        // the *set*, and a clean exit returns early from those.
        self.inner.reservation.release(self.reserved);

        let (first_death, last_gone) = {
            let mut st = lock_set(&self.set);
            // Read and write `dead` under the set lock, so the first death is
            // decided once even when two threads of a set die together.
            let already_dead = self.set.dead.load(Ordering::SeqCst);
            if self.clean && !already_dead {
                // The ordinary end of a healthy worker: teardown's `Stop`, or a
                // failed grow retiring the threads it did create. The set was
                // never a thread short, so there is nothing to account for.
                return;
            }
            // A survivor of an already-dead set comes through here too, however
            // it was asked to leave, so the count reaches zero exactly once.
            st.live_workers -= 1;
            self.set.dead.store(true, Ordering::SeqCst);
            (!already_dead, st.live_workers == 0)
        };

        if first_death {
            // A dead set never runs another job; its survivors are asked to
            // leave, and the last one out releases the slot below.
            for tx in &self.set.senders {
                let _ = tx.send(Assignment::Stop);
            }
        }
        {
            let mut reg = self.inner.lock();
            if !reg.stopping {
                // Out of `idle` on the first death, so nothing can lease a set
                // that is short a thread; out of `all` and off `created` only
                // when the last thread is gone, so the slot is released exactly
                // once and never while a dying thread still holds its stack.
                reg.idle.retain(|s| !Arc::ptr_eq(s, &self.set));
                if last_gone {
                    reg.all.retain(|s| !Arc::ptr_eq(s, &self.set));
                    reg.created -= 1;
                }
            }
        }
        if first_death {
            announce_worker_death(
                self.inner.name_prefix,
                self.set.index,
                self.inner.roster.len(),
            );
        }
    }
}

/// A worker's whole life: take a job, run it under the submitter's ambient
/// runtime inside `catch_unwind`, then return the set — every path, panic
/// included, through the one return guard.
fn worker_loop(inner: Arc<PoolInner>, set: Arc<SetHandle>, rx: Receiver<Assignment>) {
    // The band was taken by the spawned closure (the role's, for the thread's
    // whole life); the ambient runtime is the *job's*, entered per dispatch (a
    // pooled worker outlives the runtime that first used it — see
    // `InheritedRuntime`).
    while let Ok(assignment) = rx.recv() {
        match assignment {
            Assignment::Stop => break,
            Assignment::Joinable {
                body,
                ambient,
                done,
            } => {
                let outcome = ambient.run(|| catch_unwind(AssertUnwindSafe(body)));
                // The joiner gets the panic payload; if it is gone the payload
                // drops here, which only happens at teardown.
                let _ = done.send(outcome);
                finish_job(&inner, &set);
            }
            Assignment::Detached {
                body,
                ambient,
                label,
            } => {
                let outcome = ambient.run(|| catch_unwind(AssertUnwindSafe(body)));
                if outcome.is_err() {
                    announce_panic(&label);
                }
                finish_job(&inner, &set);
            }
        }
    }
}

/// One job finished: decrement `running` and re-idle the set if that freed it.
fn finish_job(inner: &Arc<PoolInner>, set: &Arc<SetHandle>) {
    let freed = {
        let mut st = lock_set(set);
        st.running -= 1;
        st.became_free()
    };
    free_if_idle(inner, set, freed);
}

// ---------------------------------------------------------------------------
// The process-wide thread-memory reservation
// ---------------------------------------------------------------------------

/// How many MiB of thread memory every pool in this process may reserve
/// *together*. Overrides [`default_reservation_budget`]; read once, on first
/// admission.
pub const POOL_RESERVATION_ENV: &str = "EPICS_RS_POOL_RESERVATION_MB";

/// Address space one pool thread reserves **beyond its declared stack**.
///
/// Measured on `x86_64-wrs-vxworks`: three arms of one image differing only in
/// the connection roster's [`StackSizeClass`] walled at 47 / 59 / 80 concurrent
/// clients as the declared per-connection stack fell 3,145,728 → 2,097,152 →
/// 1,048,576 B. Charging each thread its declared stack *plus a flat 1 MiB*
/// puts all three walls at 246.4 / 247.5 / 251.7 MB — a 2.1 % spread — while
/// charging the declared stack alone predicts a wall that never happened
/// (`doc/vxworks-ca-refusal-fidelity.md` §8; the three-arm measurement is E8's,
/// `caucus/58EWEJWV91/e8-poolprobe-0548dc61-1` §10).
///
/// It is **not** what the OS charges for a thread. A C RTP laddering bare
/// pthreads on the same guest walls at exactly `n × declared stack` — 127 × 2
/// MiB, 254 × 1 MiB, 509 × 512 KiB, each matching an `mmap` ceiling to the byte
/// (§10.2). So the flat MiB is what a *Rust* thread reserves beyond its stack,
/// consistent with a per-thread allocator arena and with E10's abort landing on
/// a 64-byte allocation inside a freshly spawned thread. Charge it per thread
/// for that reason; do not delete it on the theory that a thread costs only its
/// stack, and do not read it as an address-space constant of the target.
///
/// RTEMS takes the same figure, **assumed rather than measured**: it is the
/// conservative direction — over-charging refuses one connection early, while
/// under-charging is what walks the process into the ceiling. A host is not
/// charged, because a host's budget is unbounded anyway.
const fn per_thread_overhead(embedded: bool) -> usize {
    if embedded { 1 << 20 } else { 0 }
}

/// RTP object-arena bytes one pool thread consumes — measured, and deliberately
/// **not charged**, because a per-thread byte charge is the wrong shape for what
/// was measured.
///
/// Every VxWorks pthread mutex materialises a kernel `SEMAPHORE` object on its
/// *first lock*: `pthread_mutex_init` only stamps the magic, and
/// `pthreadMutexInit` calls `semMCreate` from `pthread_mutex_lock`, returning
/// `0x16` (`EINVAL`, not `ENOMEM`) when it comes back NULL — which `std`
/// reports as "failed to lock mutex: invalid argument (os error 22)" and
/// panics. That is the death this pool saw at the wall, and eager
/// initialisation cannot avoid it. The objects come from the RTP object arena,
/// which is **not** the address space charged above and not the allocator heap
/// either, which is why the same wall shows as an `EINVAL` in one probe and as
/// a failed 64-byte allocation in another.
///
/// E8's on-target `semMCreate` wrap then measured the exhaustion itself, on a
/// cold 1024M guest: `semMCreate` returned NULL after 588 successful creations,
/// at 49 sets / 98 workers / 48 connections — and creation **resumed past 1024**
/// afterwards. So the arena has no fixed per-thread cost to charge: it is a
/// transient rate limit, not a total, and a byte charge per thread cannot model
/// a rate. It stays `0` for that reason rather than for want of a number.
///
/// What that exhaustion does to this pool is already closed on the other side:
/// the `EINVAL` kills the worker, and the set it was holding is retired by
/// [`WorkerExit`] instead of leaking (E8 saw the leak it fixes as
/// `POOLPROBE BUSY=1 SETS=49 CONNS=0`).
const PER_THREAD_OBJECT_ARENA: usize = 0;

/// The budget when [`POOL_RESERVATION_ENV`] is unset.
///
/// Unbounded on a host: the pool's own set counts are the bound there, and no
/// host in this workspace has ever met a thread-memory wall.
///
/// 160 MiB on an embedded target, chosen from where the measured target stopped
/// *working*, not from where it stopped admitting. On the ~958 MB VxWorks guest
/// the CA pool dies at **set 46** (~230 MiB reserved, at 5 MiB a set): a
/// 64-byte allocation fails, the RTP takes signal 6 and is deleted, and no
/// refusal is delivered at all. That figure is measured on this exact code, not
/// inherited: with the budget raised to 320 MiB the same image on the same
/// guest walks to set 46 and dies there, and with the default it refuses at set
/// 32 and keeps serving (`doc/vxworks-ca-refusal-fidelity.md` §9). 160 MiB is
/// 14 sets of headroom below that — what the margin buys is that the allocator
/// and the object arena still work while the refusal is being written to the
/// socket and the console.
///
/// The ceiling itself moves with the target's RAM, 1:1: an RTP is handed
/// whatever is left after a fixed ~705 MB, measured as 254 MiB of usable address
/// space on a ~958 MB guest and 764 MiB on a ~1470 MB one. So this constant is
/// right for one box and mean to a bigger one — but it stays a constant, because
/// nothing an RTP can call reports that ceiling. `sysctl`'s `HW_PHYSMEM` and
/// `KERN_PHYSMEMTOP` answer `ENOENT`, `memFindMax`/`memInfoGet` describe a
/// 256 KiB heap partition that sits flat while the process reserves a quarter of
/// a gigabyte, `getrlimit` is in no RTP library, and `_SC_PHYS_PAGES` is not a
/// constant the RTP `unistd.h` defines. An `mmap` ladder does find the ceiling
/// exactly, but only by taking it, which in this process means another thread's
/// allocation aborts (`doc/vxworks-ca-refusal-fidelity.md` §10). Hence the
/// operator switch, and the arithmetic an operator needs for it: usable address
/// space ≈ OS memory − 705 MB, and a CA set costs 5 MiB.
///
/// RTEMS is handed the same 160 MiB and should not be: the guest this port runs
/// is a 256 MB `xilinx-zynq-a9`, so the budget is 62.5 % of the whole target and
/// the count bound or the heap is reached first. Sizing it needs an RTEMS-side
/// ladder that has not been run (§10.4).
const fn default_reservation_budget(embedded: bool) -> usize {
    if embedded { 160 << 20 } else { usize::MAX }
}

/// Parse [`POOL_RESERVATION_ENV`] (`None` = unset ⇒ `default`), with the
/// default injected so a host test can ask what an embedded process would do
/// with the same input.
///
/// A value that is not a positive whole number of MiB is ignored, with a record,
/// rather than silently becoming a bound nobody chose.
fn resolve_reservation_budget(raw: Option<&str>, default: usize) -> usize {
    let Some(raw) = raw else {
        return default;
    };
    match raw.trim().parse::<usize>() {
        Ok(mb) if mb > 0 => mb.saturating_mul(1 << 20),
        _ => {
            errlog_sev_printf(
                ErrlogSevEnum::Minor,
                &format!(
                    "{POOL_RESERVATION_ENV}={raw:?} is not a positive whole number of MiB; \
                     keeping the built-in worker-pool reservation budget"
                ),
            );
            default
        }
    }
}

/// The smallest budget the boot-time check will settle for.
///
/// A CA worker set costs ~5 MiB, so a process held to this floor admits one set
/// and refuses everything after it. That is the terminal behaviour on a target
/// that confirms no size at all: bounded and loud, rather than an `abort` at the
/// first client.
const RESERVATION_PROBE_FLOOR: usize = 8 << 20;

/// Will this target give `bytes` of address space, right now?
///
/// `Some(true)`/`Some(false)` is a measurement; `None` means this target has no
/// basis for the question and the answer must not be invented.
///
/// On VxWorks the basis is one anonymous `PROT_NONE` mapping, taken and released
/// immediately. It is the only quantity in the RTP that tracks the wall this
/// budget exists to stay under: an `mmap` ladder run at three stack classes and
/// two guest sizes reports a ceiling equal to the `pthread_create` wall **to the
/// byte**, while `memFindMax`, `memInfoGet`, `sysctl` `HW_PHYSMEM`,
/// `sysconf(_SC_PHYS_PAGES)`, `getrlimit` and `rtpInfoGet` are each blind to it
/// (`doc/vxworks-ca-refusal-fidelity.md` §10.1–§10.2). One mapping under-reads
/// that ceiling — 192 MiB confirms on a guest whose chunked ceiling is 254 MiB —
/// so it is a *lower* bound, which is the safe direction for a veto: it can
/// refuse a budget the target would in fact have honoured, never admit one it
/// would not.
///
/// Every other target answers `None`. RTEMS is not excluded for want of an
/// `mmap`; it is excluded because no RTEMS ladder has been run, so nothing is
/// known to relate a mapping there to the wall (§10.4), and a probe whose
/// relation to the resource is unmeasured is a guess wearing a measurement's
/// clothes.
#[cfg(target_os = "vxworks")]
fn address_space_admits(bytes: usize) -> Option<bool> {
    /// `sys/mman.h:66` — VxWorks requires this exact `fd` with `MAP_ANON`.
    const MAP_ANON_FD: libc::c_int = -1;

    // SAFETY: an anonymous `PROT_NONE` mapping of `bytes` at an address of the
    // kernel's choosing. Nothing is read or written through the pointer: it is
    // compared against `MAP_FAILED` and then unmapped exactly once, with the
    // same length it was created with.
    let addr = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            bytes,
            libc::PROT_NONE,
            libc::MAP_PRIVATE | libc::MAP_ANON,
            MAP_ANON_FD,
            0,
        )
    };
    if addr == libc::MAP_FAILED {
        return Some(false);
    }
    // SAFETY: `addr` came from the mapping above and is released once.
    unsafe { libc::munmap(addr, bytes) };
    Some(true)
}

#[cfg(not(target_os = "vxworks"))]
fn address_space_admits(_bytes: usize) -> Option<bool> {
    None
}

/// What the boot-time check concluded about the configured budget.
///
/// A verdict rather than a bare `usize` so that *deciding* and *saying* are
/// separate functions: the defect being closed here is a budget that kills the
/// process without a word, and a decision that carries its own announcement can
/// be asserted by a test without a `tracing` subscriber. A verdict that reaches
/// [`announce_reservation_budget`] cannot arrive silently by accident — silence
/// is one named variant, [`BudgetVerdict::Confirmed`], and nothing else.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BudgetVerdict {
    /// The target gave the configured size when asked. Nothing to say.
    Confirmed(usize),
    /// The target would not give `asked`; `adopted` is the largest size below it
    /// that the target did give.
    Clamped { asked: usize, adopted: usize },
    /// This target has nothing that measures the ceiling, so `adopted` stands
    /// unchecked. `from_env` is whether an operator chose it.
    Unverifiable { adopted: usize, from_env: bool },
    /// Nothing down to [`RESERVATION_PROBE_FLOOR`] was confirmed.
    FloorHeld { asked: usize },
}

impl BudgetVerdict {
    /// The budget this verdict adopts.
    const fn budget(self) -> usize {
        match self {
            BudgetVerdict::Confirmed(bytes)
            | BudgetVerdict::Unverifiable { adopted: bytes, .. } => bytes,
            BudgetVerdict::Clamped { adopted, .. } => adopted,
            BudgetVerdict::FloorHeld { .. } => RESERVATION_PROBE_FLOOR,
        }
    }

    /// What the operator is told at boot, if anything.
    ///
    /// A returned value rather than a call into `errlog` so a test can assert
    /// *which* outcomes are silent: on a shell-less target the whole account of
    /// the admission policy is what `errlog` said at boot, and "nothing was
    /// said" has to be a decision this function makes, not an arm somebody
    /// forgot to write.
    ///
    /// Silent in exactly two cases: the target confirmed what was configured, or
    /// the target cannot check and nobody configured anything — the built-in
    /// default carries its own measurement (see [`default_reservation_budget`])
    /// and a warning on every boot would be noise.
    fn notice(self) -> Option<(ErrlogSevEnum, String)> {
        match self {
            BudgetVerdict::Confirmed(_) => None,
            BudgetVerdict::Unverifiable {
                from_env: false, ..
            } => None,
            BudgetVerdict::Clamped { asked, adopted } => Some((
                ErrlogSevEnum::Major,
                format!(
                    "worker-pool reservation budget clamped from {} MiB to {} MiB: this target \
                     would not reserve {} MiB of address space in one mapping. Set \
                     {POOL_RESERVATION_ENV} no higher than the target can give — usable address \
                     space is what the OS leaves after keeping its own",
                    asked >> 20,
                    adopted >> 20,
                    asked >> 20
                ),
            )),
            BudgetVerdict::Unverifiable { adopted, .. } => Some((
                ErrlogSevEnum::Minor,
                format!(
                    "{POOL_RESERVATION_ENV} sets the worker-pool reservation budget to {} MiB, \
                     and this target has no measurement that tracks its thread-memory ceiling: \
                     this value cannot be verified and is taken as given",
                    adopted >> 20
                ),
            )),
            BudgetVerdict::FloorHeld { asked } => Some((
                ErrlogSevEnum::Major,
                format!(
                    "this target confirmed no worker-pool reservation budget down to {} MiB \
                     (asked for {} MiB); holding that floor, so the pool refuses nearly every \
                     client instead of exhausting the address space",
                    RESERVATION_PROBE_FLOOR >> 20,
                    asked >> 20
                ),
            )),
        }
    }
}

/// Reduce `requested` to a budget this target has been *shown* to give.
///
/// # The defect this closes
///
/// [`POOL_RESERVATION_ENV`] is the operator's only escape hatch, and it took the
/// operator at their word. Raised past what the address space can honour it does
/// not raise the ceiling — it removes the refusal that was keeping the process
/// under it: on the ~958 MB guest, `320` walks the CA pool to set 46 and the RTP
/// takes signal 6 with no refusal delivered to anyone
/// (`doc/vxworks-ca-refusal-fidelity.md` §9, §11.2). A switch that can kill the
/// IOC silently is worse than no switch.
///
/// # The rule
///
/// Adopt the largest confirmed size not above `requested`, found by halving.
/// Uniform: the built-in default is probed on exactly the same path as an
/// operator's value, because a fallback nobody measured is the same defect one
/// step down. Halving is coarse on purpose — this is a veto, not a search, and
/// an operator who wants a size between two halvings names it and has that
/// confirmed. The descent terminates at [`RESERVATION_PROBE_FLOOR`], so its cost
/// is `log2(requested / floor)` mappings, and only the first of them is paid
/// when the configured value is honest.
fn decide_reservation_budget(
    requested: usize,
    from_env: bool,
    mut admits: impl FnMut(usize) -> Option<bool>,
) -> BudgetVerdict {
    if requested == usize::MAX {
        // No wall to stay under, and no mapping of this size to ask about.
        return BudgetVerdict::Confirmed(requested);
    }
    let mut candidate = requested;
    loop {
        match admits(candidate) {
            None => {
                return BudgetVerdict::Unverifiable {
                    adopted: requested,
                    from_env,
                };
            }
            Some(true) if candidate == requested => return BudgetVerdict::Confirmed(candidate),
            Some(true) => {
                return BudgetVerdict::Clamped {
                    asked: requested,
                    adopted: candidate,
                };
            }
            Some(false) if candidate <= RESERVATION_PROBE_FLOOR => {
                return BudgetVerdict::FloorHeld { asked: requested };
            }
            Some(false) => candidate = (candidate / 2).max(RESERVATION_PROBE_FLOOR),
        }
    }
}

/// Say what the boot-time check concluded, and hand back the budget it adopts.
fn announce_reservation_budget(verdict: BudgetVerdict) -> usize {
    if let Some((severity, message)) = verdict.notice() {
        errlog_sev_printf(severity, &message);
    }
    verdict.budget()
}

/// One account of thread memory: a fixed budget and what is held against it.
///
/// A type rather than a pair of free functions so the budget can be *named* at
/// its owner — the process has exactly one account
/// ([`PROCESS_RESERVATION`]), and a test can hold its own without an
/// environment variable and without a one-shot global it cannot reset.
struct Reservation {
    budget: usize,
    held: AtomicUsize,
}

impl Reservation {
    const fn new(budget: usize) -> Self {
        Self {
            budget,
            held: AtomicUsize::new(0),
        }
    }

    /// Take `bytes`, or refuse with `(held, budget)` — the two numbers a
    /// refusal has to report.
    ///
    /// A whole set is taken in one step, before a single thread is created: the
    /// point of the budget is to refuse *before* the target is asked for memory
    /// it does not have, and a partial reservation would be no reservation.
    fn try_reserve(&self, bytes: usize) -> Result<(), (usize, usize)> {
        let mut held = self.held.load(Ordering::SeqCst);
        loop {
            let Some(next) = held.checked_add(bytes).filter(|n| *n <= self.budget) else {
                return Err((held, self.budget));
            };
            match self
                .held
                .compare_exchange_weak(held, next, Ordering::SeqCst, Ordering::SeqCst)
            {
                Ok(_) => return Ok(()),
                Err(actual) => held = actual,
            }
        }
    }

    /// Take `bytes` that cannot be refused.
    ///
    /// The pool is elastic and so it asks; a scan thread, the callback bands,
    /// the CA acceptor and the audit writer are not — an IOC that declines to
    /// create them is not an IOC. They are still *charged*, because the budget's
    /// job is to say how much room is left, and a thread the account never heard
    /// of makes that number a statement about a different process. Over-budget
    /// is representable on purpose: it makes the elastic consumer refuse sooner,
    /// which is the correct consequence of the fixed threads having taken the
    /// room.
    fn charge(&self, bytes: usize) {
        self.held.fetch_add(bytes, Ordering::SeqCst);
    }

    /// Give `bytes` back. Called once per *thread*, by the thread's own exit
    /// guard, plus once by a failed grow for the threads it never created — so
    /// the account tracks threads that exist, not sets that were planned.
    fn release(&self, bytes: usize) {
        self.held.fetch_sub(bytes, Ordering::SeqCst);
    }

    /// What the account currently holds.
    #[cfg(test)]
    fn held(&self) -> usize {
        self.held.load(Ordering::SeqCst)
    }
}

/// Thread memory every pool in this process has reserved and not yet given
/// back.
///
/// Process-wide and not per-pool because the resource is: a target that runs
/// out of address space does not care which pool reserved it, and an IOC runs
/// several (the CA server's, the CA client's, the PVA server's). A per-pool
/// budget would let three pools each stay inside their own bound and still walk
/// the process past the ceiling together.
/// Forced by the first thread the IOC charges, which on every entry point in
/// this workspace is a fixed facility thread created during start-up — so the
/// boot-time check in [`decide_reservation_budget`] and whatever it has to say
/// land before the first client, not on the first client.
static PROCESS_RESERVATION: LazyLock<Reservation> = LazyLock::new(|| {
    let default = default_reservation_budget(cfg!(epics_embedded_target));
    let requested =
        resolve_reservation_budget(std::env::var(POOL_RESERVATION_ENV).ok().as_deref(), default);
    Reservation::new(announce_reservation_budget(decide_reservation_budget(
        requested,
        requested != default,
        address_space_admits,
    )))
});

/// What one thread of `stack` reserves — the whole per-thread formula, in one
/// place, so a pool worker and a fixed IOC thread cost the account the same.
fn thread_reservation_bytes(stack: StackSizeClass) -> usize {
    stack.bytes() + per_thread_overhead(cfg!(epics_embedded_target)) + PER_THREAD_OBJECT_ARENA
}

/// What one thread of `role` reserves.
fn thread_reservation(role: &WorkerRole) -> usize {
    thread_reservation_bytes(role.stack)
}

/// The target refused to materialise a kernel object a mutex needs.
///
/// # The defect this closes
///
/// A VxWorks pthread mutex has no kernel `SEMAPHORE` until its **first lock**:
/// `pthread_mutex_init` only stamps the magic, and `pthreadMutexInit` calls
/// `semMCreate` from inside `pthread_mutex_lock`. When that returns NULL the
/// chain hands back `0x16` — `EINVAL`, not `ENOMEM` — and `std::sync::Mutex`
/// turns it into "failed to lock mutex: invalid argument (os error 22)" and
/// **panics**. Measured on target at 588 live semaphores with 49 sets / 98
/// workers / 48 connections; the panicking worker took its set with it.
///
/// It is not a total. Creation resumed past 1,024 objects after that NULL, so
/// the arena is a *transient* refusal and a byte or count budget is the wrong
/// shape for it — there is no per-thread figure to add to
/// [`PER_THREAD_OBJECT_ARENA`], and a cap set at 588 would refuse connections a
/// moment later than the target would have served them. What a transient
/// refusal needs is for the *rate* of creation to bend to the target, which is
/// what this gate does: the pool asks for the object at a point where "no"
/// costs one refusal, and the client's retry is the pacing.
///
/// # Why this is not its own [`AcquireError`] variant
///
/// It rides as the payload of [`AcquireError::SpawnFailed`], whose meaning it
/// shares exactly — *the target said no*, and the pool's own bounds were never
/// reached. A consumer that needs to tell an arena refusal from a stack refusal
/// downcasts, so the discriminator is a type rather than the message prose that
/// [`AcquireError`] exists to stop consumers parsing.
#[derive(Debug, Clone, Copy)]
pub struct ObjectArenaExhausted {
    /// How many objects the set needed — one per worker.
    pub objects: usize,
}

impl std::fmt::Display for ObjectArenaExhausted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "the target could not create the kernel mutex objects for a set of \
             {} workers; this is transient, and a client that retries will be \
             admitted once the target has objects again",
            self.objects
        )
    }
}

impl std::error::Error for ObjectArenaExhausted {}

/// The gate itself: materialise the set's own state mutex, on the thread that
/// can still refuse.
///
/// Not a throwaway probe — this is the very mutex every worker in the set locks
/// on entry and again in [`WorkerExit`], so taking its object here *removes* the
/// failure site rather than sampling near it. `try_lock` and not `lock`: the
/// mutex is one statement old and unreachable by any other thread, so `false`
/// cannot mean contention, and `try_lock` reports the target's refusal as a
/// value where `lock` would panic.
///
/// What it does not cover: the objects `std` materialises inside the *spawned*
/// thread — the parker behind a blocking `recv`, above all — which no code on
/// this side of `Builder::spawn` can take in advance. The gate narrows the
/// window and paces the burst that opens it; it does not close it, and the
/// set-retirement path in [`WorkerExit`] is what keeps the residue survivable.
fn materialise_set_mutex(state: &Mutex<SetState>) -> bool {
    state.try_lock().is_ok()
}

/// One thread's charge against the process account, held for exactly as long as
/// the thread is.
///
/// # Invariant
///
/// **MUST:** every thread this workspace creates holds one of these for its
/// whole life, pool worker or not. **MUST NOT:** any thread reserve stack the
/// account has not been told about.
///
/// # The defect this closes
///
/// The budget was an account of *pool* threads only, while an IOC also runs
/// fixed ones — the scan bands, the delayed-callback timer, the CA acceptor,
/// the audit writer, the status pusher, the dial pool's workers. Measured at
/// roughly 15 MiB on the VxWorks target: inside the headroom, and therefore
/// invisible, which is not the same as accounted for. Two things go wrong while
/// it stays invisible. The pool believes it may take the whole budget when it
/// may not, so the refusal lands later than the number says; and the moment a
/// target has more fixed threads than this one — a second server, more scan
/// rates — the error is no longer small and nothing reports that it grew.
///
/// [`Drop`] is the release, so an exit path cannot forget: the charge is moved
/// into the thread body and dies with it, including on unwind, and a
/// `Builder::spawn` that fails drops the closure and with it the charge.
pub struct ThreadCharge {
    bytes: usize,
}

impl ThreadCharge {
    /// Charge one fixed thread of `stack`. Never refuses — see
    /// [`Reservation::charge`].
    pub fn fixed(stack: StackSizeClass) -> Self {
        let bytes = thread_reservation_bytes(stack);
        PROCESS_RESERVATION.charge(bytes);
        Self { bytes }
    }
}

impl Drop for ThreadCharge {
    fn drop(&mut self) {
        PROCESS_RESERVATION.release(self.bytes);
    }
}

/// Why [`WorkerPool::acquire`] refused.
///
/// # The defect this closes
///
/// `acquire` refuses at two gates that mean opposite things to whoever has to
/// act on the refusal:
///
/// * [`AtCapacity`](Self::AtCapacity) — *this process* said no. Every set the
///   pool may create is already leased. The remedy is to raise the bound (or to
///   accept the bound as the connection limit it is); the target is fine.
/// * [`OutOfReservation`](Self::OutOfReservation) — *this process* said no on
///   behalf of the target: admitting would reserve more thread memory than the
///   process is allowed to hold. The remedy is RAM plus a raised
///   [`POOL_RESERVATION_ENV`], and the target is still healthy — which is the
///   whole point of refusing here rather than one connection later.
/// * [`SpawnFailed`](Self::SpawnFailed) — *the target* said no. The OS refused
///   to create the set's threads. The remedy is memory, and the pool's own
///   bound is irrelevant because it was never reached.
///
/// Both used to be an `io::Error`, and both landed on `io::ErrorKind::WouldBlock`
/// — the capacity arm by construction, the spawn arm because a failed
/// `Builder::spawn` is `EAGAIN` and `std` decodes `EAGAIN` as `WouldBlock`. So
/// the one discriminator a consumer had was the message *prose*, and every
/// consumer that branched on `kind()` silently answered the wrong question.
/// Both server drivers did: the CA server reported both as one status on the
/// wire — measured on VxWorks 7, where both gates were reached on one image
/// with `available=48` on each (`doc/vxworks-ca-refusal-fidelity.md` §6) — and
/// the PVA server's `kind() == WouldBlock` arm reports an out-of-threads target
/// as `max_connections reached`, naming a bound that never fired. That second
/// one is by construction, not measured: the blocking PVA server has not been
/// driven to its wall on this target.
///
/// Naming the gate in the type is what makes that class of mistake unwritable:
/// a consumer that wants "is this the connection limit" must now say so, and
/// gets an answer that cannot be an `EAGAIN` in disguise.
///
/// The [`From`] conversion to `io::Error` keeps each variant's historical
/// `ErrorKind` for callers that only propagate, and carries `self` as the
/// error's payload so the gate survives the conversion and stays recoverable
/// with `downcast_ref`.
#[derive(Debug)]
pub enum AcquireError {
    /// Every set the pool may ever create is leased out. `capacity` is the
    /// bound that was reached — the number to report and the number to raise.
    AtCapacity {
        /// The pool's declared capacity, in sets.
        capacity: usize,
    },
    /// Admitting would take the process past its thread-memory budget. Nothing
    /// was reserved and no thread was created.
    OutOfReservation {
        /// What this set would have reserved, in bytes.
        requested: usize,
        /// Already reserved by every pool in the process, in bytes.
        reserved: usize,
        /// The process budget, in bytes — the number [`POOL_RESERVATION_ENV`]
        /// raises.
        budget: usize,
    },
    /// The OS refused to create the set's threads. The pool was below its
    /// capacity and `created` is left exactly as it was found.
    SpawnFailed(io::Error),
    /// The pool is shutting down and will not lease again.
    ShuttingDown,
}

impl std::fmt::Display for AcquireError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            // The leading words are load-bearing: they are what the on-target
            // consoles and `doc/vxworks-ca-refusal-fidelity.md` already carry,
            // so an operator's existing grep keeps working.
            AcquireError::AtCapacity { capacity } => {
                write!(f, "worker pool at capacity ({capacity} sets)")
            }
            AcquireError::OutOfReservation {
                requested,
                reserved,
                budget,
            } => write!(
                f,
                "worker pool at its thread-memory budget: this set needs {} KiB, \
                 {} of {} MiB already reserved — raise {POOL_RESERVATION_ENV} \
                 if the target has the memory",
                requested / 1024,
                reserved >> 20,
                budget >> 20,
            ),
            AcquireError::SpawnFailed(e) => write!(f, "cannot create a worker set: {e}"),
            AcquireError::ShuttingDown => write!(f, "worker pool is shutting down"),
        }
    }
}

impl std::error::Error for AcquireError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            AcquireError::SpawnFailed(e) => Some(e),
            AcquireError::AtCapacity { .. }
            | AcquireError::OutOfReservation { .. }
            | AcquireError::ShuttingDown => None,
        }
    }
}

impl From<AcquireError> for io::Error {
    fn from(cause: AcquireError) -> io::Error {
        let kind = match &cause {
            // The reservation gate is the capacity gate's twin — the process
            // refusing, not the target — so it keeps the same historical kind
            // for callers that only propagate; the variant is what tells them
            // apart.
            AcquireError::AtCapacity { .. } | AcquireError::OutOfReservation { .. } => {
                io::ErrorKind::WouldBlock
            }
            // The OS's own kind, so a propagating caller sees what the target
            // actually said rather than a re-labelling of it.
            AcquireError::SpawnFailed(e) => e.kind(),
            AcquireError::ShuttingDown => io::ErrorKind::BrokenPipe,
        };
        io::Error::new(kind, cause)
    }
}

/// A bounded, per-role set of persistent threads that connections borrow.
///
/// `N` is the set size — three for the PVA server (`conn`, `reader`, `writer`),
/// two for a blocking client's circuit (`reader`, `writer`). `Worker` and
/// `SetLease` are **not** generic over `N`, so a leased worker crosses into the
/// byte-pump seam without spreading a const parameter through every signature.
pub struct WorkerPool<const N: usize> {
    inner: Arc<PoolInner>,
}

impl<const N: usize> WorkerPool<N> {
    /// Declare a role's pool. Lazy: no thread exists until the first
    /// [`acquire`](Self::acquire) that cannot reuse an idle set.
    ///
    /// `capacity` is the most sets that may ever exist, and for a server it is
    /// the connection limit — admission refuses past it. Not `const`, because a
    /// pool owns heap state; a process-lifetime pool is a `LazyLock<WorkerPool>`,
    /// a server-lifetime pool is a field dropped with the server.
    pub fn new(name_prefix: &'static str, roster: [WorkerRole; N], capacity: usize) -> Self {
        Self::with_reservation(name_prefix, roster, capacity, &PROCESS_RESERVATION)
    }

    /// [`Self::new`] against a named account, so a test can bound a pool by a
    /// budget of its own choosing without touching the process's.
    fn with_reservation(
        name_prefix: &'static str,
        roster: [WorkerRole; N],
        capacity: usize,
        reservation: &'static Reservation,
    ) -> Self {
        Self::with_reservation_and_gate(
            name_prefix,
            roster,
            capacity,
            reservation,
            materialise_set_mutex,
        )
    }

    /// [`Self::with_reservation`] with the object-arena gate injected, so a host
    /// test can exercise the refusal a target produces.
    fn with_reservation_and_gate(
        name_prefix: &'static str,
        roster: [WorkerRole; N],
        capacity: usize,
        reservation: &'static Reservation,
        materialise: fn(&Mutex<SetState>) -> bool,
    ) -> Self {
        let set_reservation = roster.iter().map(thread_reservation).sum();
        Self {
            inner: Arc::new(PoolInner {
                roster: Box::new(roster),
                name_prefix,
                capacity,
                set_reservation,
                reservation,
                materialise,
                reg: Mutex::new(Registry {
                    idle: VecDeque::new(),
                    all: Vec::new(),
                    created: 0,
                    joins: Vec::new(),
                    stopping: false,
                }),
            }),
        }
    }

    /// Borrow a whole set, or refuse.
    ///
    /// * an idle set exists → reuse it (no thread created);
    /// * none, and `created < capacity` → grow by one set (`N` threads);
    /// * none, and at capacity → [`AcquireError::AtCapacity`] carrying the
    ///   bound that was reached;
    /// * a thread could not be created → [`AcquireError::SpawnFailed`], with
    ///   `created` left exactly as it was found.
    ///
    /// The refusals are a sum type and not an `io::Error` because they mean
    /// opposite things — a full process versus a target out of thread resources
    /// — and as `io::Error` they were indistinguishable: both are
    /// `ErrorKind::WouldBlock`. See [`AcquireError`].
    pub fn acquire(&self) -> Result<(SetLease, [Worker; N]), AcquireError> {
        // Decide under the pool lock, spawn without it: a reserved-then-spawn
        // step keeps `created` an exact bound without holding the lock across a
        // thread creation.
        enum Decision {
            Reuse(Arc<SetHandle>),
            Grow(usize),
            Full,
        }
        let decision = {
            let mut reg = self.inner.lock();
            if reg.stopping {
                return Err(AcquireError::ShuttingDown);
            }
            if let Some(set) = reg.idle.pop_front() {
                Decision::Reuse(set)
            } else if reg.created < self.inner.capacity {
                let index = reg.created;
                reg.created += 1;
                Decision::Grow(index)
            } else {
                Decision::Full
            }
        };

        let set = match decision {
            Decision::Full => {
                return Err(AcquireError::AtCapacity {
                    capacity: self.inner.capacity,
                });
            }
            Decision::Reuse(set) => set,
            Decision::Grow(index) => {
                // The memory this set will hold is taken from the process
                // budget *before* the target is asked to create anything, so a
                // refusal happens while the target is still healthy enough to
                // deliver it. Reusing an idle set reserves nothing: its threads
                // already exist and are already charged.
                if let Err((reserved, budget)) = self
                    .inner
                    .reservation
                    .try_reserve(self.inner.set_reservation)
                {
                    self.inner.lock().created -= 1;
                    return Err(AcquireError::OutOfReservation {
                        requested: self.inner.set_reservation,
                        reserved,
                        budget,
                    });
                }
                match self.spawn_set(index) {
                    Ok((set, joins)) => {
                        let mut reg = self.inner.lock();
                        reg.joins.extend(joins);
                        reg.all.push(set.clone());
                        set
                    }
                    Err(e) => {
                        // The slot reservation is given back so a later attempt
                        // may grow again; the memory reservation of the threads
                        // that were never created was given back by `spawn_set`,
                        // and the ones that were created give theirs back as
                        // they exit. Nothing else changed.
                        self.inner.lock().created -= 1;
                        return Err(AcquireError::SpawnFailed(e));
                    }
                }
            }
        };

        // Lease it: leased, not parked. A grown set starts with these values;
        // a reused one is flipped back to them here.
        {
            let mut st = lock_set(&set);
            st.leased = true;
            st.parked = false;
        }
        let workers: Vec<Worker> = (0..N)
            .map(|slot| Worker {
                inner: self.inner.clone(),
                set: set.clone(),
                tx: set.senders[slot].clone(),
            })
            .collect();
        let workers: [Worker; N] = workers
            .try_into()
            .unwrap_or_else(|_| unreachable!("N workers for an N-role set"));
        let lease = SetLease {
            inner: self.inner.clone(),
            set,
        };
        Ok((lease, workers))
    }

    /// Spawn one set's `N` threads. On a partway failure the threads already
    /// created are stopped and joined, so a failed grow leaks nothing.
    fn spawn_set(&self, index: usize) -> io::Result<(Arc<SetHandle>, Vec<JoinHandle<()>>)> {
        let mut senders = Vec::with_capacity(N);
        let mut receivers = Vec::with_capacity(N);
        for _ in 0..N {
            let (tx, rx) = channel::<Assignment>();
            senders.push(tx);
            receivers.push(rx);
        }
        let set = Arc::new(SetHandle {
            index,
            senders,
            dead: AtomicBool::new(false),
            state: Mutex::new(SetState {
                leased: false,
                running: 0,
                parked: false,
                // The set's full roster. A grow that fails partway never
                // publishes the set and retires its threads through `Stop`, so
                // no short-staffed set is ever counted here.
                live_workers: N,
            }),
        });

        // The object-arena gate. Nothing has been created yet, so refusing here
        // costs the caller a refusal and the target nothing.
        if !(self.inner.materialise)(&set.state) {
            let unspawned: usize = self.inner.roster.iter().map(thread_reservation).sum();
            self.inner.reservation.release(unspawned);
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                ObjectArenaExhausted { objects: N },
            ));
        }

        let mut joins: Vec<JoinHandle<()>> = Vec::with_capacity(N);
        for (slot, rx) in receivers.into_iter().enumerate() {
            let role = self.inner.roster[slot];
            let name = format!("{}-{} {index}", self.inner.name_prefix, role.suffix);
            let inner = self.inner.clone();
            let set_for_worker = set.clone();
            let spawned = thread::Builder::new()
                .name(name)
                .stack_size(role.stack.bytes())
                .spawn(move || {
                    // Installed before anything that can unwind — the prologue
                    // included — so no way out of this thread leaves the set
                    // counted against the pool's bound. See `WorkerExit`.
                    let mut exit = WorkerExit {
                        inner: inner.clone(),
                        set: set_for_worker.clone(),
                        reserved: thread_reservation(&role),
                        clean: false,
                    };
                    // The band is the role's, for the thread's whole life. Taken
                    // here in the closure so the crate's thread-prologue guards
                    // see it on the spawned body.
                    let _ = enter_ioc_thread(role.priority);
                    worker_loop(inner, set_for_worker, rx);
                    exit.clean = true;
                });
            match spawned {
                Ok(handle) => joins.push(handle),
                Err(e) => {
                    // The threads from this slot on do not exist and never
                    // will, so their share of the set's reservation is given
                    // back here — the ones that *were* created give theirs back
                    // through their own exit guards, so every byte is released
                    // exactly once by whoever it was spent on.
                    let unspawned: usize = self.inner.roster[joins.len()..]
                        .iter()
                        .map(thread_reservation)
                        .sum();
                    self.inner.reservation.release(unspawned);
                    // Retire the workers already spawned for this set. Their
                    // senders live in `set`, still held here, so the `Stop`s land.
                    for tx in &set.senders {
                        let _ = tx.send(Assignment::Stop);
                    }
                    for handle in joins {
                        let _ = handle.join();
                    }
                    return Err(e);
                }
            }
        }
        Ok((set, joins))
    }

    /// Threads this pool has created, ever — never more than
    /// `capacity × N`. The bound made observable: the number the per-connection
    /// shape grew without limit.
    pub fn worker_count(&self) -> usize {
        self.inner.lock().created * N
    }

    /// `(busy_sets, created_sets, capacity)` — the admission state.
    ///
    /// Deliberately not `queue_depth`: there is no queue, admission refuses, and
    /// a name that promised one would be the dual meaning this design removes.
    pub fn set_usage(&self) -> (usize, usize, usize) {
        let reg = self.inner.lock();
        let busy = reg.created - reg.idle.len();
        (busy, reg.created, self.inner.capacity)
    }
}

impl<const N: usize> Drop for WorkerPool<N> {
    /// Retire every worker thread. A process-lifetime pool (a `static`
    /// `LazyLock`) never reaches here; a server-lifetime pool does, at server
    /// drop, and must be dropped *after* the server's connections have been
    /// asked to stop, so the `Stop`s do not queue behind a live connection
    /// forever.
    fn drop(&mut self) {
        let (senders, joins) = {
            let mut reg = self.inner.lock();
            reg.stopping = true;
            reg.idle.clear();
            let joins = std::mem::take(&mut reg.joins);
            // One `Stop` per worker across *every* set — leased or idle — so no
            // worker is left parked on `recv`. `all` is what makes a leased set's
            // senders reachable here; the idle deque alone would miss them.
            let mut senders: Vec<Sender<Assignment>> = Vec::new();
            for set in &reg.all {
                senders.extend(set.senders.iter().cloned());
            }
            (senders, joins)
        };
        // An idle worker takes its `Stop` at once; a worker still inside a job
        // takes it after that job returns. A correct teardown has already asked
        // its connections to stop, so no `Stop` waits behind a live one forever.
        for tx in senders {
            let _ = tx.send(Assignment::Stop);
        }
        for handle in joins {
            let _ = handle.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    fn roster2() -> [WorkerRole; 2] {
        [
            WorkerRole {
                suffix: "reader",
                stack: StackSizeClass::Small,
                priority: ThreadPriority::Low,
            },
            WorkerRole {
                suffix: "writer",
                stack: StackSizeClass::Small,
                priority: ThreadPriority::Low,
            },
        ]
    }

    /// A set is borrowed and returned; the next borrow reuses the same threads.
    ///
    /// The tight spot is the borrow immediately after a return: the set is freed
    /// by the last job's completion, and a pool that counted parked workers
    /// instead of busy ones would fail to see it as available. So the assertion
    /// is inside the loop, not only after it — the direct statement of the
    /// closed leak.
    #[test]
    fn sequential_borrows_reuse_one_set() {
        let pool: WorkerPool<2> = WorkerPool::new("test-pool", roster2(), 4);
        const BORROWS: usize = 8;
        for i in 0..BORROWS {
            let (lease, [reader, writer]) = pool.acquire().expect("borrow");
            let ran = Arc::new(AtomicUsize::new(0));
            let r = ran.clone();
            let jr = reader.run(move || {
                r.fetch_add(1, Ordering::SeqCst);
            });
            let w = ran.clone();
            let jw = writer.run(move || {
                w.fetch_add(1, Ordering::SeqCst);
            });
            assert!(jr.join().is_ok());
            assert!(jw.join().is_ok());
            drop(lease);
            // The set is freed by the last job's completion / the lease drop,
            // both of which may still be settling; wait for the return.
            let deadline = Instant::now() + Duration::from_secs(5);
            while pool.set_usage().0 != 0 {
                assert!(Instant::now() < deadline, "set never returned to idle");
                thread::yield_now();
            }
            assert_eq!(ran.load(Ordering::SeqCst), 2);
            assert_eq!(
                pool.worker_count(),
                2,
                "borrow {i} created new threads instead of reusing the idle set"
            );
        }
        assert_eq!(
            pool.worker_count(),
            2,
            "{BORROWS} sequential borrows must have created exactly one set"
        );
    }

    /// A set is not reused while any of its jobs is still running.
    #[test]
    fn a_set_is_not_reidled_while_a_job_runs() {
        let pool: WorkerPool<2> = WorkerPool::new("test-hold", roster2(), 4);
        let (lease, [reader, writer]) = pool.acquire().expect("borrow");
        // A job that blocks until released.
        let gate = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
        let g = gate.clone();
        let blocking = reader.run(move || {
            let (m, cv) = &*g;
            let mut open = m.lock().unwrap();
            while !*open {
                open = cv.wait(open).unwrap();
            }
        });
        let quick = writer.run(|| {});
        assert!(quick.join().is_ok());
        drop(lease);
        // Lease gone and one job done, but the reader is still running: the set
        // must NOT be idle.
        assert_eq!(
            pool.set_usage().0,
            1,
            "a running job must keep its set busy"
        );
        // Release the blocked job.
        {
            let (m, cv) = &*gate;
            *m.lock().unwrap() = true;
            cv.notify_all();
        }
        assert!(blocking.join().is_ok());
        let deadline = Instant::now() + Duration::from_secs(5);
        while pool.set_usage().0 != 0 {
            assert!(
                Instant::now() < deadline,
                "set never returned after its last job"
            );
            thread::yield_now();
        }
        assert_eq!(pool.worker_count(), 2);
    }

    /// At capacity, `acquire` refuses by naming the bound and creates no thread.
    #[test]
    fn acquire_refuses_at_capacity_without_creating_a_thread() {
        let pool: WorkerPool<2> = WorkerPool::new("test-cap", roster2(), 1);
        let (lease, _workers) = pool.acquire().expect("first borrow");
        let before = pool.worker_count();
        let refused = pool.acquire().err();
        assert!(
            matches!(refused, Some(AcquireError::AtCapacity { capacity: 1 })),
            "a full pool must refuse by naming the bound it reached, not queue \
             or grow: {refused:?}"
        );
        assert_eq!(
            pool.worker_count(),
            before,
            "a refusal must create no thread"
        );
        drop(lease);
    }

    /// # Invariant
    ///
    /// MUST: a refusal name which gate refused. MUST NOT: "this process is
    /// full" and "this target is out of threads" be the same value.
    ///
    /// They were, and the collapse is `std`'s, not ours: a failed
    /// `Builder::spawn` is `EAGAIN`, `std` decodes `EAGAIN` as
    /// `ErrorKind::WouldBlock`, and the capacity refusal was constructed as
    /// `WouldBlock` too. So a consumer branching on `kind()` — as the PVA
    /// accept path did — could not tell a full server from a target that had
    /// run out of thread resources, and reported the second as the first.
    ///
    /// The first assertion is the collapse itself, asserted rather than
    /// described, so this test still states *why* the type exists if `std` ever
    /// changes that mapping.
    #[test]
    fn a_full_pool_and_a_refused_spawn_are_not_the_same_refusal() {
        let eagain = io::Error::from_raw_os_error(11);
        assert_eq!(
            eagain.kind(),
            io::ErrorKind::WouldBlock,
            "EAGAIN decodes as WouldBlock — the collapse this type exists to \
             undo. If this ever stops holding, say so here rather than in a \
             comment."
        );

        let pool: WorkerPool<2> = WorkerPool::new("test-gate", roster2(), 1);
        let (lease, _workers) = pool.acquire().expect("first borrow");
        let full = pool.acquire().err().expect("the pool is full");
        let spawn_failed = AcquireError::SpawnFailed(io::Error::from_raw_os_error(11));

        assert!(
            matches!(full, AcquireError::AtCapacity { .. }),
            "a full pool is a capacity refusal: {full:?}"
        );
        assert!(
            !matches!(spawn_failed, AcquireError::AtCapacity { .. }),
            "a refused spawn must never present as the capacity gate: it is \
             the difference between 'raise the bound' and 'add memory'"
        );
        // …and the distinction survives the lossy conversion, so even a caller
        // that only ever sees `io::Error` can recover the gate.
        let as_io: io::Error = full.into();
        assert_eq!(
            as_io.kind(),
            io::ErrorKind::WouldBlock,
            "the historical kind is preserved for callers that only propagate"
        );
        assert!(
            matches!(
                as_io
                    .get_ref()
                    .and_then(|e| e.downcast_ref::<AcquireError>()),
                Some(AcquireError::AtCapacity { capacity: 1 })
            ),
            "the gate must survive the io::Error conversion: {as_io:?}"
        );
        drop(lease);
    }

    /// A job that panics returns its set, and the worker keeps serving: the next
    /// borrow succeeds and no thread was created to replace the one that
    /// panicked.
    #[test]
    fn a_panicked_job_returns_its_set_and_the_worker_survives() {
        let pool: WorkerPool<2> = WorkerPool::new("test-panic", roster2(), 2);
        let (lease, [reader, writer]) = pool.acquire().expect("borrow");
        let boom = reader.run(|| panic!("job blew up"));
        let ok = writer.run(|| {});
        assert!(boom.join().is_err(), "the panic must reach the joiner");
        assert!(ok.join().is_ok());
        drop(lease);
        let deadline = Instant::now() + Duration::from_secs(5);
        while pool.set_usage().0 != 0 {
            assert!(Instant::now() < deadline, "panicked set never returned");
            thread::yield_now();
        }
        let created_before = pool.worker_count();
        // The same threads serve the next borrow.
        let (lease2, [r2, w2]) = pool.acquire().expect("borrow after panic");
        assert!(r2.run(|| {}).join().is_ok());
        assert!(w2.run(|| {}).join().is_ok());
        drop(lease2);
        assert_eq!(
            pool.worker_count(),
            created_before,
            "a lost worker is never recreated, and a survivor needs no replacement"
        );
    }

    /// A detached job runs and returns its set with no joiner.
    #[test]
    fn a_detached_job_returns_its_set() {
        let pool: WorkerPool<2> = WorkerPool::new("test-detach", roster2(), 2);
        let (lease, [reader, writer]) = pool.acquire().expect("borrow");
        let ran = Arc::new(AtomicUsize::new(0));
        let r = ran.clone();
        reader.run_detached("conn".into(), move || {
            r.fetch_add(1, Ordering::SeqCst);
        });
        let done = writer.run(|| {});
        assert!(done.join().is_ok());
        drop(lease);
        let deadline = Instant::now() + Duration::from_secs(5);
        while pool.set_usage().0 != 0 {
            assert!(Instant::now() < deadline, "detached set never returned");
            thread::yield_now();
        }
        assert_eq!(ran.load(Ordering::SeqCst), 1);
    }

    /// A panic payload whose own `Drop` panics — the deterministic stand-in for
    /// a worker thread that dies somewhere the job's `catch_unwind` does not
    /// cover.
    struct PanicOnDrop;

    impl Drop for PanicOnDrop {
        fn drop(&mut self) {
            panic!("payload drop: the worker thread dies here, outside catch_unwind");
        }
    }

    /// # Invariant
    ///
    /// MUST: a set whose worker thread has exited be retired — released from
    /// `busy`, dropped from the idle deque, and given back to `created`. MUST
    /// NOT: a worker's death leave its set counted busy for the life of the
    /// process.
    ///
    /// Measured on `x86_64-wrs-vxworks` at the reservation wall: three worker
    /// threads died across two sets and the pool reported `BUSY=2 SETS=50
    /// WORKERS=100 CONNS=0` — two sets leased forever with no client attached,
    /// so the connection bound was permanently 139 instead of 141 and every
    /// further death cost another set
    /// (`doc/vxworks-ca-worker-pool-on-target-measurement.md` §14, on
    /// `caucus/58EWEJWV91/e8-poolprobe-0548dc61-1`).
    ///
    /// The target's mechanism was a `std` mutex lock returning `EINVAL` inside
    /// the loop's return path, which is not reproducible on demand. This
    /// reproduces the *same* thread death at the *same* point deterministically:
    /// the job's panic is caught, and then the payload is dropped on the worker
    /// thread — `let _ = done.send(outcome)` drops it there when the joiner is
    /// already gone — so the worker unwinds before it reaches `finish_job`.
    /// Any panic on that stretch does this; the payload is only how the test
    /// gets one on demand.
    #[test]
    fn a_worker_that_dies_retires_its_set_instead_of_leaking_it() {
        let pool: WorkerPool<2> = WorkerPool::new("test-dead", roster2(), 2);
        let (lease, [reader, _writer]) = pool.acquire().expect("borrow");

        // The body waits, so the `Job` can be dropped first: the worker's
        // `done.send` must fail for the payload to drop on the worker thread.
        let (go, wait) = channel::<()>();
        let job = reader.run(move || {
            let _ = wait.recv();
            std::panic::panic_any(PanicOnDrop);
        });
        drop(job);
        drop(lease);
        go.send(()).expect("the worker is waiting on this");
        drop(go);

        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let (busy, created, _cap) = pool.set_usage();
            if busy == 0 {
                assert_eq!(
                    created, 0,
                    "a set with a dead worker must not stay countable: its \
                     threads are gone, so its slot must return to the bound"
                );
                break;
            }
            assert!(
                Instant::now() < deadline,
                "the set is still busy with no lease and no live job: a worker \
                 that died took its set out of circulation permanently, which \
                 is the one-set-per-death leak measured on target"
            );
            thread::yield_now();
        }

        // And the pool still admits: the retired set freed its slot.
        let (lease2, [r2, w2]) = pool.acquire().expect("borrow after a death");
        assert!(
            r2.run(|| {}).join().is_ok(),
            "a fresh set must actually run"
        );
        assert!(w2.run(|| {}).join().is_ok());
        drop(lease2);
    }

    /// The other side of the death boundary: the lease is still held when the
    /// worker dies. Retirement may not wait for the borrower — the slot has to
    /// come back while the borrower still holds its (now useless) lease, and the
    /// lease drop that follows must not push a thread-short set back to idle.
    #[test]
    fn a_death_under_a_live_lease_returns_the_slot_and_never_repools_the_set() {
        let pool: WorkerPool<2> = WorkerPool::new("test-dead-leased", roster2(), 2);
        let (lease, [reader, writer]) = pool.acquire().expect("borrow");

        // Same handshake as above: the payload must drop on the *worker*, so the
        // `Job` has to be gone before the body panics.
        let (go, wait) = channel::<()>();
        let job = reader.run(move || {
            let _ = wait.recv();
            std::panic::panic_any(PanicOnDrop);
        });
        drop(job);
        go.send(()).expect("the worker is waiting on this");
        drop(go);

        let deadline = Instant::now() + Duration::from_secs(10);
        while pool.set_usage().1 != 0 {
            assert!(
                Instant::now() < deadline,
                "a set that lost a thread stayed countable while its lease was \
                 held; the slot must return as soon as the threads are gone"
            );
            thread::yield_now();
        }

        // The surviving role is unusable, and says so rather than reporting a
        // body that never ran as a clean completion.
        assert!(
            writer.run(|| {}).join().is_err(),
            "a job dispatched into a retired set must be reported as not run"
        );

        drop(lease);
        assert_eq!(
            pool.set_usage(),
            (0, 0, 2),
            "the lease drop must not re-pool a retired set"
        );
        assert!(
            pool.acquire().is_ok(),
            "the pool must still admit after a death under lease"
        );
    }

    /// Dropping the pool retires its worker threads rather than leaking them.
    #[test]
    fn dropping_the_pool_joins_its_workers() {
        let pool: WorkerPool<2> = WorkerPool::new("test-drop", roster2(), 2);
        let (lease, [reader, writer]) = pool.acquire().expect("borrow");
        assert!(reader.run(|| {}).join().is_ok());
        assert!(writer.run(|| {}).join().is_ok());
        drop(lease);
        // Give the set time to return so `drop` finds the workers idle.
        let deadline = Instant::now() + Duration::from_secs(5);
        while pool.set_usage().0 != 0 {
            assert!(Instant::now() < deadline, "set never returned before drop");
            thread::yield_now();
        }
        // Must not hang: the `Stop`s reach idle workers and the join completes.
        drop(pool);
    }

    /// One set of [`roster2`] on a 64-bit host: two `Small` stacks and no
    /// per-thread overhead charged off-target.
    const HOST_SET: usize = 2 * 512 * 1024;

    /// # Invariant
    ///
    /// MUST: admission refuse while the thread memory it would reserve is still
    /// unspent. MUST NOT: a pool create a thread whose memory is not already
    /// reserved from the budget.
    ///
    /// The defect: the pool's only bound was a *count*, so on
    /// `x86_64-wrs-vxworks` the CA server walked to 41 concurrent clients and
    /// the RTP died — a 64-byte allocation failed, `signal 6`, whole process
    /// gone — with its count bound of 141 nowhere in sight
    /// (`doc/vxworks-ca-refusal-fidelity.md` §6.3). A bound reached after the
    /// target has run out is not an admission gate.
    ///
    /// The boundary is exact rather than narrative: a budget of two sets admits
    /// two and refuses the third, and the refusal costs no thread.
    #[test]
    fn admission_refuses_at_the_memory_budget_before_the_count_bound() {
        static TWO_SETS: Reservation = Reservation::new(2 * HOST_SET);
        // Capacity 8 so the count bound cannot be what refuses.
        let pool: WorkerPool<2> =
            WorkerPool::with_reservation("test-budget", roster2(), 8, &TWO_SETS);

        let (l1, _w1) = pool.acquire().expect("first set fits");
        let (l2, _w2) = pool.acquire().expect("second set fits exactly");
        assert_eq!(pool.worker_count(), 4, "two sets, two threads each");

        let refused = pool.acquire().err().expect("the third set does not fit");
        assert!(
            matches!(
                refused,
                AcquireError::OutOfReservation {
                    requested,
                    reserved,
                    budget,
                } if requested == HOST_SET
                    && reserved == 2 * HOST_SET
                    && budget == 2 * HOST_SET
            ),
            "the refusal must name what was asked for, what is held and the \
             budget — the three numbers the remedy needs: {refused:?}"
        );
        assert_eq!(
            pool.worker_count(),
            4,
            "a refusal must not have created the threads it refused"
        );
        assert_eq!(
            pool.set_usage(),
            (2, 2, 8),
            "the refused grow must leave the slot reservation exactly as it \
             found it"
        );

        drop(l1);
        drop(l2);
        // Returning a set does not return its memory: its threads still exist.
        // What must come back is the *reuse*, and it does — the fourth borrow
        // creates nothing.
        let (l3, _w3) = pool.acquire().expect("an idle set is reused, not grown");
        assert_eq!(pool.worker_count(), 4);
        drop(l3);
        drop(pool);
        assert_eq!(
            TWO_SETS.held.load(Ordering::SeqCst),
            0,
            "every thread's reservation must come back when the pool is dropped"
        );
    }

    /// The release side of the same invariant on the path that has no `Drop` of
    /// its own to lean on: a set whose worker *died*. Its memory must return to
    /// the budget, or a target that loses a worker refuses connections it has
    /// the memory to serve — for the life of the process.
    #[test]
    fn a_dead_set_gives_its_memory_back_to_the_budget() {
        static ONE_SET: Reservation = Reservation::new(HOST_SET);
        let pool: WorkerPool<2> = WorkerPool::with_reservation("test-rel", roster2(), 4, &ONE_SET);

        let (lease, [reader, _writer]) = pool.acquire().expect("the one set fits");
        let (go, wait) = channel::<()>();
        let job = reader.run(move || {
            let _ = wait.recv();
            std::panic::panic_any(PanicOnDrop);
        });
        drop(job);
        drop(lease);
        go.send(()).expect("the worker is waiting on this");
        drop(go);

        let deadline = Instant::now() + Duration::from_secs(10);
        while ONE_SET.held.load(Ordering::SeqCst) != 0 {
            assert!(
                Instant::now() < deadline,
                "a set that lost a worker kept its reservation: held {} of {}",
                ONE_SET.held.load(Ordering::SeqCst),
                HOST_SET
            );
            thread::yield_now();
        }
        pool.acquire()
            .expect("the budget freed by the dead set must admit a new one");
    }

    /// The budget's two policy inputs, both arms of each, without needing the
    /// target that selects them.
    #[test]
    fn the_reservation_budget_reads_its_switch_and_its_target_default() {
        assert_eq!(
            default_reservation_budget(false),
            usize::MAX,
            "a host is not bounded by thread memory"
        );
        assert_eq!(
            default_reservation_budget(true),
            160 * 1024 * 1024,
            "the embedded default is the measured one; changing it is a \
             behaviour change and must be stated here"
        );
        assert_eq!(per_thread_overhead(false), 0);
        assert_eq!(
            per_thread_overhead(true),
            1024 * 1024,
            "the flat per-thread reservation measured on VxWorks 7"
        );

        let default = default_reservation_budget(true);
        assert_eq!(resolve_reservation_budget(None, default), default);
        assert_eq!(resolve_reservation_budget(Some("8"), default), 8 << 20);
        assert_eq!(resolve_reservation_budget(Some(" 12 "), default), 12 << 20);
        // A value that is not a budget leaves the default standing rather than
        // becoming a bound nobody chose.
        assert_eq!(resolve_reservation_budget(Some("0"), default), default);
        assert_eq!(resolve_reservation_budget(Some("lots"), default), default);
        assert_eq!(resolve_reservation_budget(Some(""), default), default);
    }

    /// A probe that answers from a table and records what it was asked.
    fn probe<'a>(
        answers: &'static [(usize, Option<bool>)],
        asked: &'a mut Vec<usize>,
    ) -> impl FnMut(usize) -> Option<bool> + 'a {
        move |bytes| {
            asked.push(bytes);
            answers
                .iter()
                .find(|(size, _)| *size == bytes)
                .map(|(_, answer)| *answer)
                .unwrap_or(Some(false))
        }
    }

    /// # Invariant
    ///
    /// MUST: the adopted reservation budget be one the target answered for, or
    /// else be announced as unverified. MUST NOT: a configured budget reach the
    /// pool without either a confirmation or a notice.
    ///
    /// The defect: `EPICS_RS_POOL_RESERVATION_MB` was the operator's only escape
    /// hatch and was taken at face value. Raised past what the address space can
    /// honour it does not add memory, it deletes the refusal that was keeping the
    /// process below the wall — 320 MiB on the ~958 MB guest walks the CA pool to
    /// set 46, and the RTP takes `signal 6` with no refusal delivered
    /// (`doc/vxworks-ca-refusal-fidelity.md` §9, §11.2).
    ///
    /// One case per boundary of the descent, not per story.
    #[test]
    fn a_configured_budget_is_confirmed_clamped_or_declared_unverifiable() {
        // Host: no wall, and `usize::MAX` is not a mapping anyone can ask about,
        // so the probe is not even consulted.
        let mut asked = Vec::new();
        assert_eq!(
            decide_reservation_budget(usize::MAX, false, probe(&[], &mut asked)),
            BudgetVerdict::Confirmed(usize::MAX)
        );
        assert!(asked.is_empty(), "no mapping is asked for on a host");

        // The target gives what was configured: one mapping, adopted as asked,
        // and nothing is said.
        let mut asked = Vec::new();
        assert_eq!(
            decide_reservation_budget(
                160 << 20,
                false,
                probe(&[(160 << 20, Some(true))], &mut asked)
            ),
            BudgetVerdict::Confirmed(160 << 20)
        );
        assert_eq!(asked, vec![160 << 20], "an honest value costs one mapping");

        // The measured case: 320 MiB configured on the ~958 MB guest, whose
        // single-mapping bound is between 192 and 256 MiB (§10.2/§10.3). The
        // descent rejects 320 and adopts 160.
        let mut asked = Vec::new();
        assert_eq!(
            decide_reservation_budget(
                320 << 20,
                true,
                probe(
                    &[(320 << 20, Some(false)), (160 << 20, Some(true))],
                    &mut asked
                )
            ),
            BudgetVerdict::Clamped {
                asked: 320 << 20,
                adopted: 160 << 20
            }
        );
        assert_eq!(asked, vec![320 << 20, 160 << 20]);

        // No basis: the value stands, and stands *declared*. `from_env` is the
        // whole difference between a notice and silence.
        let mut asked = Vec::new();
        assert_eq!(
            decide_reservation_budget(320 << 20, true, probe(&[(320 << 20, None)], &mut asked)),
            BudgetVerdict::Unverifiable {
                adopted: 320 << 20,
                from_env: true
            }
        );
        assert_eq!(asked, vec![320 << 20], "one question, then no more");
        let mut asked = Vec::new();
        assert_eq!(
            decide_reservation_budget(160 << 20, false, probe(&[(160 << 20, None)], &mut asked)),
            BudgetVerdict::Unverifiable {
                adopted: 160 << 20,
                from_env: false
            }
        );
    }

    /// The descent's own boundaries: it ends *on* the floor, never below it, and
    /// a target that confirms nothing leaves the pool bounded rather than dead.
    #[test]
    fn the_budget_descent_terminates_on_the_floor() {
        // Nothing is confirmed. The descent halves to the floor and stops there.
        let mut asked = Vec::new();
        assert_eq!(
            decide_reservation_budget(64 << 20, true, probe(&[], &mut asked)),
            BudgetVerdict::FloorHeld { asked: 64 << 20 }
        );
        assert_eq!(
            asked,
            vec![64 << 20, 32 << 20, 16 << 20, 8 << 20],
            "halving, and the last question is the floor itself"
        );

        // A halving that would undershoot lands on the floor instead of below
        // it: 10 MiB / 2 is 5 MiB, which is not a size worth asking about.
        let mut asked = Vec::new();
        assert_eq!(
            decide_reservation_budget(
                10 << 20,
                true,
                probe(&[(RESERVATION_PROBE_FLOOR, Some(true))], &mut asked)
            ),
            BudgetVerdict::Clamped {
                asked: 10 << 20,
                adopted: RESERVATION_PROBE_FLOOR
            }
        );
        assert_eq!(asked, vec![10 << 20, RESERVATION_PROBE_FLOOR]);

        // Configured below the floor and refused: one question, and the floor is
        // held rather than the descent running past it.
        let mut asked = Vec::new();
        assert_eq!(
            decide_reservation_budget(4 << 20, true, probe(&[], &mut asked)),
            BudgetVerdict::FloorHeld { asked: 4 << 20 }
        );
        assert_eq!(asked, vec![4 << 20]);
    }

    /// # Invariant
    ///
    /// MUST NOT: any outcome but "the target confirmed what was configured"
    /// reach the pool without an `errlog` line. The defect being closed is a
    /// budget that kills the process without a word, so silence has to be a
    /// decision this code makes rather than an arm nobody wrote — the `match` is
    /// exhaustive so a new verdict cannot compile until it is classified.
    #[test]
    fn every_verdict_but_confirmation_is_announced() {
        for verdict in [
            BudgetVerdict::Confirmed(160 << 20),
            BudgetVerdict::Clamped {
                asked: 320 << 20,
                adopted: 160 << 20,
            },
            BudgetVerdict::Unverifiable {
                adopted: 320 << 20,
                from_env: true,
            },
            BudgetVerdict::Unverifiable {
                adopted: 160 << 20,
                from_env: false,
            },
            BudgetVerdict::FloorHeld { asked: 320 << 20 },
        ] {
            let notice = verdict.notice();
            match verdict {
                BudgetVerdict::Confirmed(bytes) => {
                    assert_eq!(notice, None, "an honoured budget is not news");
                    assert_eq!(verdict.budget(), bytes);
                }
                BudgetVerdict::Unverifiable {
                    adopted,
                    from_env: false,
                } => {
                    assert_eq!(
                        notice, None,
                        "the built-in default carries its own measurement"
                    );
                    assert_eq!(verdict.budget(), adopted);
                }
                BudgetVerdict::Unverifiable { adopted, .. } => {
                    let (severity, message) = notice.expect("an unverified value must say so");
                    assert_eq!(severity, ErrlogSevEnum::Minor);
                    assert!(
                        message.contains("cannot be verified")
                            && message.contains(POOL_RESERVATION_ENV),
                        "the notice must name the switch and its own uncertainty: {message}"
                    );
                    assert_eq!(verdict.budget(), adopted);
                }
                BudgetVerdict::Clamped { asked, adopted } => {
                    let (severity, message) = notice.expect("a clamp must say so");
                    assert_eq!(
                        severity,
                        ErrlogSevEnum::Major,
                        "the IOC is not doing what the switch said"
                    );
                    assert!(
                        message.contains(&format!("{} MiB", asked >> 20))
                            && message.contains(&format!("{} MiB", adopted >> 20)),
                        "both numbers, or the operator cannot tell what happened: {message}"
                    );
                    assert_eq!(verdict.budget(), adopted);
                }
                BudgetVerdict::FloorHeld { asked } => {
                    let (severity, message) = notice.expect("a held floor must say so");
                    assert_eq!(severity, ErrlogSevEnum::Major);
                    assert!(
                        message.contains(&format!("{} MiB", asked >> 20)),
                        "the notice must name what was asked for: {message}"
                    );
                    assert_eq!(verdict.budget(), RESERVATION_PROBE_FLOOR);
                }
            }
        }
    }

    /// # Invariant
    ///
    /// MUST: a thread the IOC cannot decline to create still charge the one
    /// account the pool spends from, for exactly as long as it runs. MUST NOT:
    /// any thread's stack be invisible to the number admission divides.
    ///
    /// The defect: the budget counted pool workers only, while the same target
    /// runs the scan bands, the callback timer, the CA acceptor, the audit
    /// writer and the dial pool's workers — about 15 MiB on the VxWorks guest.
    /// Being inside the headroom is not the same as being counted: the pool
    /// believed it could take the whole budget, and nothing would have reported
    /// the error growing on a target with more fixed threads.
    #[test]
    fn a_fixed_thread_charges_the_process_account_and_gives_it_back() {
        let before = PROCESS_RESERVATION.held();
        let expect = thread_reservation_bytes(StackSizeClass::Small);
        {
            let _charge = ThreadCharge::fixed(StackSizeClass::Small);
            assert_eq!(
                PROCESS_RESERVATION.held(),
                before + expect,
                "a fixed thread must appear in the account the pool divides"
            );
        }
        assert_eq!(
            PROCESS_RESERVATION.held(),
            before,
            "the charge is released by the guard's `Drop`, not by a caller"
        );
    }

    /// The charge is tied to the *thread*, not to the call that started it.
    ///
    /// The boundary that matters: the account must still hold the stack while
    /// the thread runs, and must be back to where it started once the thread
    /// has ended — which is what makes a long-lived fixed thread reduce what
    /// the pool may take, and a finished one give it back.
    #[test]
    fn the_spawn_helper_holds_its_charge_for_the_thread_and_not_the_call() {
        use crate::runtime::task::spawn_dedicated_thread;

        let before = PROCESS_RESERVATION.held();
        let expect = thread_reservation_bytes(StackSizeClass::Small);
        let (release, wait) = channel::<()>();
        let (started, running) = channel::<()>();

        let handle = spawn_dedicated_thread(
            "charged-fixed-thread".to_string(),
            ThreadPriority::Low,
            StackSizeClass::Small,
            move || {
                let _ = started.send(());
                let _ = wait.recv();
            },
        )
        .expect("the host can create one thread");

        running.recv().expect("the thread starts");
        assert_eq!(
            PROCESS_RESERVATION.held(),
            before + expect,
            "the account must hold the stack while the thread runs"
        );

        drop(release);
        handle.join().expect("the thread ends cleanly");
        assert_eq!(
            PROCESS_RESERVATION.held(),
            before,
            "and must be back where it started once the thread is gone"
        );
    }

    /// # Invariant
    ///
    /// MUST: a target that cannot materialise a set's mutex object refuse the
    /// connection. MUST NOT: the pool create a thread that will meet that
    /// refusal as a `std` panic, or keep the memory of a set it did not build.
    ///
    /// The defect this pins: on VxWorks every pthread mutex materialises its
    /// `SEMAPHORE` on first lock, so a freshly leased worker's first
    /// `std::sync::Mutex::lock` panicked with `EINVAL` at 588 live objects and
    /// took its set with it. A host cannot be made to exhaust that arena, so
    /// the gate is injected — what is under test is the *refusal path*: no
    /// thread, no leaked byte, no leaked slot, and a cause a consumer can
    /// recognise by type.
    #[test]
    fn a_target_that_refuses_a_mutex_object_refuses_the_connection() {
        static ARENA: Reservation = Reservation::new(8 * HOST_SET);
        fn arena_empty(_: &Mutex<SetState>) -> bool {
            false
        }
        let pool: WorkerPool<2> =
            WorkerPool::with_reservation_and_gate("test-arena", roster2(), 8, &ARENA, arena_empty);
        let before = ARENA.held();

        let refused = pool.acquire().err().expect("the arena refuses the set");
        let AcquireError::SpawnFailed(ref e) = refused else {
            panic!("an arena refusal is the target saying no: {refused:?}");
        };
        let arena = e
            .get_ref()
            .and_then(|src| src.downcast_ref::<ObjectArenaExhausted>())
            .expect("the cause must be recognisable by type, not by prose");
        assert_eq!(arena.objects, 2, "one object per worker in the set");
        assert_eq!(
            e.kind(),
            io::ErrorKind::WouldBlock,
            "a transient refusal is retryable, and a client's retry is the pacing"
        );

        assert_eq!(pool.worker_count(), 0, "a refusal must create no thread");
        assert_eq!(
            ARENA.held(),
            before,
            "the set's memory must go back: it was reserved for threads that do \
             not exist"
        );

        // And the slot: a refused grow must not consume capacity, or eight
        // transient refusals would close a pool of eight for good.
        let ok: WorkerPool<2> = WorkerPool::with_reservation_and_gate(
            "test-arena-recovers",
            roster2(),
            1,
            &ARENA,
            materialise_set_mutex,
        );
        let _lease = ok.acquire().expect("a target with objects admits");
        assert!(pool.acquire().is_err(), "still refusing");
    }
}
