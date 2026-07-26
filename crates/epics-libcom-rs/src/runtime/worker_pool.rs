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

use std::collections::VecDeque;
use std::io;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::mpsc::{Receiver, Sender, SyncSender, channel, sync_channel};
use std::sync::{Arc, Mutex};
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
}

impl SetState {
    /// Mutate under the lock, then answer *did this transition free the set?* —
    /// true at most once per lease, because `parked` latches.
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
    /// One sender per role, cloned into the [`Worker`]s handed out at lease.
    senders: Vec<Sender<Assignment>>,
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
    set: Arc<SetHandle>,
    tx: Sender<Assignment>,
}

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
        // A closed channel means the worker is gone at teardown; the send below
        // fails and the job simply never ran, which the pool's own shutdown
        // ordering makes unreachable while any lease is outstanding.
        let _ = self.tx.send(Assignment::Joinable {
            body: Box::new(body),
            ambient: InheritedRuntime::capture(),
            done,
        });
        Job { done: done_rx }
    }

    /// Dispatch a job nobody will join. The worker announces a panic through
    /// `errlog` under `label`; a clean return is silent.
    pub fn run_detached<F>(self, label: String, body: F)
    where
        F: FnOnce() + Send + 'static,
    {
        self.charge();
        let _ = self.tx.send(Assignment::Detached {
            body: Box::new(body),
            ambient: InheritedRuntime::capture(),
            label,
        });
    }
}

/// A handle to a running job, joined on the borrower's teardown path.
pub struct Job {
    done: Receiver<thread::Result<()>>,
}

impl Job {
    /// Block until the job returns, yielding whether it panicked.
    ///
    /// A dropped sender (the worker gone at teardown) reads as a clean
    /// completion: there is no unwind to report and nothing to tear down twice.
    pub fn join(self) -> thread::Result<()> {
        self.done.recv().unwrap_or(Ok(()))
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
            AcquireError::SpawnFailed(e) => write!(f, "cannot create a worker set: {e}"),
            AcquireError::ShuttingDown => write!(f, "worker pool is shutting down"),
        }
    }
}

impl std::error::Error for AcquireError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            AcquireError::SpawnFailed(e) => Some(e),
            AcquireError::AtCapacity { .. } | AcquireError::ShuttingDown => None,
        }
    }
}

impl From<AcquireError> for io::Error {
    fn from(cause: AcquireError) -> io::Error {
        let kind = match &cause {
            AcquireError::AtCapacity { .. } => io::ErrorKind::WouldBlock,
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
        Self {
            inner: Arc::new(PoolInner {
                roster: Box::new(roster),
                name_prefix,
                capacity,
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
            Decision::Grow(index) => match self.spawn_set(index) {
                Ok((set, joins)) => {
                    let mut reg = self.inner.lock();
                    reg.joins.extend(joins);
                    reg.all.push(set.clone());
                    set
                }
                Err(e) => {
                    // The reservation is given back so a later attempt may grow
                    // again; nothing else changed.
                    self.inner.lock().created -= 1;
                    return Err(AcquireError::SpawnFailed(e));
                }
            },
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
            senders,
            state: Mutex::new(SetState {
                leased: false,
                running: 0,
                parked: false,
            }),
        });

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
                    // The band is the role's, for the thread's whole life. Taken
                    // here in the closure so the crate's thread-prologue guards
                    // see it on the spawned body.
                    let _ = enter_ioc_thread(role.priority);
                    worker_loop(inner, set_for_worker, rx);
                });
            match spawned {
                Ok(handle) => joins.push(handle),
                Err(e) => {
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
}
