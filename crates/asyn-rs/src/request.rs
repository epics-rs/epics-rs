//! Request types for the port actor.

use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering as AtomicOrdering};

use parking_lot::Mutex;
use std::time::SystemTime;

use epics_libcom_rs::SharedArray;

use crate::error::{AsynError, AsynResult, AsynStatus, QueueRefusal};
use crate::param::ParamValue;
use crate::port::PortDriver;

/// A closure a request carries to the actor — [`DriverCall`] or [`PortLockCall`].
///
/// A request is `Clone`, a closure runs once: the clones share one slot and the
/// first to reach the actor takes it. A call with nothing left to run — a second
/// clone, or one rebuilt from a serialized `PortCommand`, which cannot carry
/// code — is refused by the actor rather than silently succeeding.
pub struct OnceCall<F: ?Sized>(Arc<Mutex<Option<Box<F>>>>);

/// The closure a [`RequestOp::WithDriver`] carries.
pub type DriverCall = OnceCall<dyn FnOnce(&mut dyn PortDriver) + Send>;

/// The closure a [`RequestOp::LockPort`] carries.
pub type PortLockCall = OnceCall<dyn FnOnce(&mut dyn PortCalls) + Send>;

impl DriverCall {
    pub fn new(f: impl FnOnce(&mut dyn PortDriver) + Send + 'static) -> Self {
        Self(Arc::new(Mutex::new(Some(Box::new(f)))))
    }
}

impl PortLockCall {
    pub fn new(f: impl FnOnce(&mut dyn PortCalls) + Send + 'static) -> Self {
        Self(Arc::new(Mutex::new(Some(Box::new(f)))))
    }
}

impl<F: ?Sized> OnceCall<F> {
    /// A call with no closure, which the actor refuses.
    pub fn spent() -> Self {
        Self(Arc::new(Mutex::new(None)))
    }

    pub(crate) fn take(&self) -> Option<Box<F>> {
        self.0.lock().take()
    }
}

impl<F: ?Sized> Clone for OnceCall<F> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl<F: ?Sized> std::fmt::Debug for OnceCall<F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("OnceCall")
    }
}

/// The port as a [`RequestCell`] body sees it: each call goes straight
/// to the driver, as C's callback calls the interfaces it holds, with no
/// further `queueRequest`. The driver reads `op` and its data in place, as C
/// passes the record's own buffer, so a body keeps one op's buffer from
/// process to process.
pub trait PortCalls {
    fn call(&mut self, op: &RequestOp, user: crate::user::AsynUser) -> AsynResult<RequestResult>;

    /// An octet read on the port, its bytes beside its status.
    fn octet_read(&mut self, op: OctetReadOp, user: crate::user::AsynUser) -> OctetRead {
        octet_outcome(self.call(&op.into_op(), user))
    }
}

/// Why a queued [`RequestCell`] left its queue without running — the three
/// ways C's `userPvt` is unlinked short of `processUser`.
#[derive(Debug)]
pub enum Unrun {
    /// C `cancelRequest` while `isQueued` (asynManager.c:1663-1668).
    Canceled,
    /// C `queueTimeoutCallback` (asynManager.c:649-703).
    TimedOut,
    /// The port refused it at dequeue, or went away with it queued.
    Refused(AsynError),
}

/// What a [`RequestCell`] runs — C `createAsynUser(processUser, timeoutUser)`,
/// fixed when the cell is made, so a queue carries nothing but the cell.
pub trait CellBody: Send + Sync + 'static {
    /// C `processUser`, on the port's thread, with the port.
    fn process(&self, port: &mut dyn PortCalls);
    /// C `timeoutUser`, or the other ways the request never ran.
    fn unrun(&self, why: Unrun);
    /// Whether C's `createAsynUser` was given a `timeoutUser`. Without one
    /// (`false`) a request on a disconnected device runs [`Self::process`]
    /// (asynManager.c:886), and a queue timeout is refused at queue time
    /// (:1592-1597), so only a cancel or a port shutdown reaches `unrun`.
    fn timeout_user(&self) -> bool {
        true
    }
    /// Runs after each `process` or `unrun` returns. The cell may already be
    /// queued again by then, from inside the callback, as C allows.
    fn done(self: Arc<Self>);
}

/// One preallocated request — C's `asynUser` and its `userPvt`, which
/// `queueRequest` links onto a port queue without allocating
/// (asynManager.c:1605-1628).
///
/// One request per cell is queued, as C's `isQueued` allows one per `asynUser`
/// (asynManager.c:1586-1590). A request leaves the queue before its callback
/// runs, so the callback may queue the cell again (:684 before :689, :895
/// before :905).
pub struct RequestCell {
    queue: Arc<CellQueue>,
    token: CancelToken,
    /// The block identity of every request this cell queues and every call its
    /// body makes — one per cell, as C's is one per `asynUser`.
    block_token: u64,
    slot: Mutex<Slot>,
    body: Arc<dyn CellBody>,
}

/// The cell's request and its state, which change together.
struct Slot {
    request: CellRequest,
    state: CellState,
}

/// C's `isQueued` and `state == callbackActive`, as one state. `callbacks`
/// counts the `process`/`unrun` calls running: one that queued the cell again
/// can still be running when that request leaves the queue too.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CellState {
    Idle,
    Queued,
    Active { callbacks: u32 },
    ActiveQueued { callbacks: u32 },
}

impl CellState {
    fn is_queued(self) -> bool {
        matches!(self, Self::Queued | Self::ActiveQueued { .. })
    }

    /// The request leaves the queue and its callback is about to run.
    fn dequeue(&mut self) {
        *self = match *self {
            Self::Queued => Self::Active { callbacks: 1 },
            Self::ActiveQueued { callbacks } => Self::Active {
                callbacks: callbacks + 1,
            },
            idle => unreachable!("{idle:?} has no request to dequeue"),
        };
    }

    /// A callback starts without the cell leaving a queue — C `queueRequest`
    /// on a synchronous port, which tests no `isQueued` (asynManager.c:1555).
    fn enter_inline(&mut self) {
        *self = match *self {
            Self::Idle => Self::Active { callbacks: 1 },
            Self::Active { callbacks } => Self::Active {
                callbacks: callbacks + 1,
            },
            Self::Queued => Self::ActiveQueued { callbacks: 1 },
            Self::ActiveQueued { callbacks } => Self::ActiveQueued {
                callbacks: callbacks + 1,
            },
        };
    }

    /// A callback [`Self::dequeue`] started has returned.
    fn end_callback(&mut self) {
        *self = match *self {
            Self::Active { callbacks: 1 } => Self::Idle,
            Self::Active { callbacks } => Self::Active {
                callbacks: callbacks - 1,
            },
            Self::ActiveQueued { callbacks: 1 } => Self::Queued,
            Self::ActiveQueued { callbacks } => Self::ActiveQueued {
                callbacks: callbacks - 1,
            },
            idle => unreachable!("{idle:?} has no callback running"),
        };
    }
}

/// What the actor needs of a queued cell: the gate's user and its queue band
/// and order. `seq` is also the request's generation: an actor entry whose
/// `seq` is not the cell's is a request that already left, and decides nothing.
#[derive(Clone, Copy)]
pub(crate) struct CellRequest {
    pub reason: usize,
    pub addr: i32,
    pub priority: crate::port::QueuePriority,
    pub seq: u64,
}

/// What the actor may do with a dequeued cell entry — [`RequestCell::claim`].
pub(crate) enum Claim {
    /// C `callbackActive`: run it.
    Run,
    /// It left the queue and the actor reports it ([`RequestCell::leave`]).
    Left,
    /// It left and was reported, or a later request owns the cell now.
    Gone,
}

impl RequestCell {
    /// C `queueRequest` past its port gate: link the cell and signal the port
    /// thread. Refused, with C's text, only while the cell is queued
    /// (asynManager.c:1586-1590); a callback still running does not refuse it.
    pub fn queue(
        self: &Arc<Self>,
        user: &crate::user::AsynUser,
    ) -> Result<(), QueueRefusal<'static>> {
        // All of it under the lock, as C does under `asynManagerLock`: an exit
        // that finds the cell queued finds its timer and its entry too.
        let mut slot = self.lock();
        let was = slot.state;
        slot.state = match was {
            CellState::Idle => CellState::Queued,
            CellState::Active { callbacks } => CellState::ActiveQueued { callbacks },
            CellState::Queued | CellState::ActiveQueued { .. } => {
                return Err(QueueRefusal::AlreadyQueued);
            }
        };
        if user.queue_timeout.is_some() && !self.body.timeout_user() {
            slot.state = was;
            return Err(QueueRefusal::NoTimeoutUser);
        }
        let seq = crate::port_actor::next_seq();
        slot.request = CellRequest {
            reason: user.reason,
            addr: user.addr,
            priority: user.priority,
            seq,
        };
        self.token.requeue();
        // C starts the timer before it signals the port thread
        // (asynManager.c:1619-1628).
        if let Some(timeout) = user.queue_timeout {
            queue_timer().arm(self, seq, std::time::Instant::now() + timeout.get());
        }
        if self.queue.push(self, seq) {
            Ok(())
        } else {
            queue_timer().disarm(self);
            slot.state = was;
            self.token.leave_if_queued();
            Err(QueueRefusal::ShutDown)
        }
    }

    /// C `cancelRequest` (asynManager.c:1663-1668): `true` (C `wasQueued`)
    /// when the cell was still queued, which unlinks it at once, runs its body's
    /// [`Unrun::Canceled`] and `done` here — asynRecord's `special()` reports and
    /// requests its callback inline (asynRecord.c:396-404) — and tells the port
    /// thread, whose entry for it is now gone (`is_gone`).
    pub fn cancel(&self) -> bool {
        {
            let mut slot = self.lock();
            if !slot.state.is_queued() || !self.token.cancel() {
                return false;
            }
            queue_timer().disarm(self);
            self.queue.unlink(self);
            slot.state.dequeue();
        }
        self.body.unrun(Unrun::Canceled);
        self.end_callback();
        self.queue.poke();
        true
    }

    /// C `blockProcessCallback(pasynUser, allDevices)` (asynManager.c:1694-1725)
    /// on this cell's block identity, from any thread. The block holds once
    /// this cell's next callback returns, or the one running now.
    pub fn block_process_callback(&self, all_devices: bool) -> AsynResult<()> {
        if !self.queue.can_block {
            return Err(block_error(
                "asynManager::blockProcessCallback blockProcessCallback \
                 not supported because port is synchronous",
            ));
        }
        let slot = self.lock();
        if slot.state.is_queued() {
            return Err(block_error("asynManager::blockProcessCallback is queued"));
        }
        self.queue
            .blocks()
            .lock()
            .block(self.block_token, all_devices);
        Ok(())
    }

    /// C `unblockProcessCallback(pasynUser, allDevices)`
    /// (asynManager.c:1727-1786), from any thread — the port thread inside this
    /// cell's own callback included. Releasing the holder wakes the actor.
    pub fn unblock_process_callback(&self, all_devices: bool) -> AsynResult<()> {
        let slot = self.lock();
        let mut blocks = self.queue.blocks().lock();
        if blocks.count(self.block_token, all_devices) == 0 {
            return Err(block_error(
                "asynManager::unblockProcessCallback but not locked",
            ));
        }
        if slot.state.is_queued() {
            return Err(block_error("asynManager::unblockProcessCallback is queued"));
        }
        let dp = crate::port_actor::DpKey::of(self.queue.multi_device, slot.request.addr);
        let was_owner = blocks
            .unblock(self.block_token, dp, all_devices)
            .map_err(block_error)?;
        drop(blocks);
        drop(slot);
        if was_owner {
            self.queue.nudge();
        }
        Ok(())
    }

    /// [`CellBody::timeout_user`].
    pub(crate) fn timeout_user(&self) -> bool {
        self.body.timeout_user()
    }

    pub(crate) fn queues_on(&self, queue: &Arc<CellQueue>) -> bool {
        Arc::ptr_eq(&self.queue, queue)
    }

    /// Whether it rides C's `asynQueuePriorityConnect` queue.
    pub fn connect_queue(&self) -> bool {
        self.request().priority == crate::port::QueuePriority::Connect
    }

    /// C `isQueued`, or a callback still running.
    #[cfg(feature = "epics")]
    pub(crate) fn is_outstanding(&self) -> bool {
        self.lock().state != CellState::Idle
    }

    pub(crate) fn token(&self) -> &CancelToken {
        &self.token
    }

    pub(crate) fn request(&self) -> CellRequest {
        self.lock().request
    }

    pub(crate) fn block_token(&self) -> u64 {
        self.block_token
    }

    fn lock(&self) -> parking_lot::MutexGuard<'_, Slot> {
        self.slot.lock()
    }

    /// The port thread's dequeue of the entry queued as `seq` (C
    /// asynManager.c:894-895, and the timer cancel at :908).
    pub(crate) fn claim(&self, seq: u64) -> Claim {
        let mut slot = self.lock();
        if self.left(&slot, seq) {
            Claim::Gone
        } else if self.token.begin_running() {
            queue_timer().disarm(self);
            slot.state.dequeue();
            Claim::Run
        } else {
            Claim::Left
        }
    }

    /// On the port's thread, once [`Self::claim`] said [`Claim::Run`].
    pub(crate) fn run(&self, port: &mut dyn PortCalls) {
        self.body.process(port);
        self.token.finish();
        self.end_callback();
    }

    /// C `processUser` run by `queueRequest` itself, on a port without
    /// `ASYN_CANBLOCK` ([`crate::port_handle::PortHandle::queue_request`]).
    pub(crate) fn run_inline(&self, port: &mut dyn PortCalls) {
        self.lock().state.enter_inline();
        self.body.process(port);
        self.end_callback();
    }

    /// The actor takes the entry queued as `seq` off its queue without running
    /// it, for `e`: reported once, by whichever exit wins the token, and not at
    /// all for an entry a later request has replaced.
    pub(crate) fn leave(&self, seq: u64, e: AsynError) {
        {
            let mut slot = self.lock();
            // A later request owns the cell, or the canceller or the timer
            // reported this one.
            if self.left(&slot, seq) || !self.token.leave_if_queued() {
                return;
            }
            queue_timer().disarm(self);
            slot.state.dequeue();
        }
        self.body
            .unrun(if matches!(e, AsynError::QueueTimeout { .. }) {
                Unrun::TimedOut
            } else {
                Unrun::Refused(e)
            });
        self.end_callback();
    }

    /// C `queueTimeoutCallback` (asynManager.c:649-703), on the timer thread:
    /// the request queued as `seq` leaves the queue while the port thread may be
    /// inside the driver, its timeout callback runs, and the port thread is
    /// told (:702).
    fn expire(&self, seq: u64) {
        {
            let mut slot = self.lock();
            // :657-663 `if(!puserPvt->isQueued) return;`
            if self.left(&slot, seq) || !self.token.time_out_if_queued() {
                return;
            }
            // :664-674 unlinks it and :684 clears `isQueued` before :689 calls
            // `timeoutUser`; one entry the actor already took is its to drop.
            self.queue.unlink(self);
            slot.state.dequeue();
        }
        self.body.unrun(Unrun::TimedOut);
        self.end_callback();
        self.queue.poke();
    }

    /// Whether the actor's entry queued as `seq` is one it can drop unseen: a
    /// later request owns the cell, or a cancel or the timer took this one.
    pub(crate) fn is_gone(&self, seq: u64) -> bool {
        self.left(&self.lock(), seq)
    }

    /// [`Self::is_gone`] under the lock.
    fn left(&self, slot: &Slot, seq: u64) -> bool {
        slot.request.seq != seq || !slot.state.is_queued()
    }

    fn end_callback(&self) {
        self.lock().state.end_callback();
        Arc::clone(&self.body).done();
    }
}

impl Drop for RequestCell {
    /// A block this cell still counts or holds ends with it. C does not:
    /// ~AsynDriverInterface's cancelRequest (AsynDriverInterface.cc:308)
    /// leaves the counts (asynManager.c:1632-1690), its disconnect refuses
    /// while `blockProcessCallback is active` (:1377-1380) and its
    /// freeAsynUser (AsynDriverInterface.cc:315-316) fails with it
    /// (:1245-1246), so the asynUser leaks and the port stays blocked for
    /// good, a `streamReload` included. That defect is not copied.
    fn drop(&mut self) {
        if self.queue.blocks().lock().release(self.block_token) {
            self.queue.nudge();
        }
        self.queue.unregister();
    }
}

impl std::fmt::Debug for RequestCell {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("RequestCell")
    }
}

fn block_error(message: &str) -> AsynError {
    AsynError::Status {
        status: AsynStatus::Error,
        message: message.into(),
    }
}

/// A port's queue of [`RequestCell`]s as the queueing side sees it: a push
/// into capacity every registered cell reserved, and a wake of the actor —
/// C's `ellAdd` and `epicsEventSignal(notifyPortThread)`
/// (asynManager.c:1615, :1628).
#[derive(Default)]
pub(crate) struct CellQueue {
    inner: Mutex<CellQueueInner>,
    wake: tokio::sync::Notify,
    /// A request left the queue on someone else's clock — the timer's or a
    /// waiter's — and the actor still holds its entry.
    left: std::sync::atomic::AtomicBool,
    /// The port's block state, under the lock C keeps it under
    /// (`asynManagerLock`), so a block is counted and released from any
    /// thread.
    blocks: crate::port_actor::Blocks,
    /// `ASYN_CANBLOCK`: C refuses a block on a synchronous port (:1708-1713).
    can_block: bool,
    multi_device: bool,
}

#[derive(Default)]
struct CellQueueInner {
    /// Each entry with the `seq` it was queued as.
    cells: std::collections::VecDeque<(Arc<RequestCell>, u64)>,
    registered: usize,
    closed: bool,
}

impl CellQueue {
    pub(crate) fn for_port(can_block: bool, multi_device: bool) -> Self {
        Self {
            can_block,
            multi_device,
            ..Self::default()
        }
    }

    pub(crate) fn blocks(&self) -> &crate::port_actor::Blocks {
        &self.blocks
    }

    /// A cell for this queue. Each cell reserves its own slot, so queueing one
    /// never grows the queue.
    pub(crate) fn new_cell(self: &Arc<Self>, body: Arc<dyn CellBody>) -> Arc<RequestCell> {
        {
            let mut inner = self.inner.lock();
            inner.registered += 1;
            let spare = inner.registered - inner.cells.len();
            inner.cells.reserve(spare);
        }
        queue_timer().register();
        Arc::new(RequestCell {
            queue: Arc::clone(self),
            token: CancelToken::new(),
            block_token: crate::user::next_block_token(),
            slot: Mutex::new(Slot {
                request: CellRequest {
                    reason: 0,
                    addr: -1,
                    priority: crate::port::QueuePriority::default(),
                    seq: 0,
                },
                state: CellState::Idle,
            }),
            body,
        })
    }

    fn unregister(&self) {
        let mut inner = self.inner.lock();
        inner.registered -= 1;
        drop(inner);
        queue_timer().unregister();
    }

    fn push(&self, cell: &Arc<RequestCell>, seq: u64) -> bool {
        {
            let mut inner = self.inner.lock();
            if inner.closed {
                return false;
            }
            inner.cells.push_back((Arc::clone(cell), seq));
        }
        self.wake.notify_one();
        true
    }

    fn unlink(&self, cell: &RequestCell) {
        let mut inner = self.inner.lock();
        inner
            .cells
            .retain(|(queued, _)| !std::ptr::eq(Arc::as_ptr(queued), cell));
    }

    pub(crate) fn pop(&self) -> Option<(Arc<RequestCell>, u64)> {
        self.inner.lock().cells.pop_front()
    }

    /// Wake the actor with nothing queued — for a store it owes announces for.
    pub(crate) fn nudge(&self) {
        self.wake.notify_one();
    }

    /// Resolves at the next push or poke since the last wait.
    pub(crate) async fn pushed(&self) {
        self.wake.notified().await
    }

    /// C `queueTimeoutCallback`'s `epicsEventSignal(pport->notifyPortThread)`
    /// (asynManager.c:702): a request left the queue off the port thread.
    pub(crate) fn poke(&self) {
        self.left.store(true, std::sync::atomic::Ordering::Release);
        self.wake.notify_one();
    }

    /// Whether a request left since the last call.
    pub(crate) fn take_left(&self) -> bool {
        self.left.swap(false, std::sync::atomic::Ordering::AcqRel)
    }

    /// The actor is gone: refuse what is queued and everything queued later.
    pub(crate) fn close(&self) {
        let cells = {
            let mut inner = self.inner.lock();
            inner.closed = true;
            std::mem::take(&mut inner.cells)
        };
        for (cell, seq) in cells {
            cell.leave(
                seq,
                AsynError::QueueRefused {
                    status: AsynStatus::Error,
                    message: "port shut down before the request ran".into(),
                },
            );
        }
    }
}

/// C `pasynBase->timerQueue` (asynManager.c:501-502): the one queue whose
/// thread fires every queued [`RequestCell`]'s queue-wait timer, so a request
/// times out while its port's thread is inside the driver.
///
/// Each cell holds one slot from creation on, as C's `userPvt` holds its
/// `epicsTimer` (:1189-1190), so arming one never allocates.
struct QueueTimer {
    slots: epics_libcom_rs::runtime::sync::PriorityInheritanceMutex<TimerSlots>,
    wake: epics_libcom_rs::runtime::sync::Event,
}

#[derive(Default)]
struct TimerSlots {
    armed: Vec<Armed>,
    registered: usize,
    /// The deadline the thread is parked until, while it is parked on one: an
    /// arm at or after it needs no wake.
    parked_until: Option<std::time::Instant>,
}

struct Armed {
    at: std::time::Instant,
    seq: u64,
    cell: Arc<RequestCell>,
}

/// Holds the queue timer's lock on another thread for `hold`, returning once
/// it is held: an arm in that window waits on the lock.
#[cfg(all(test, feature = "epics"))]
pub(crate) fn hold_queue_timer_for_test(hold: std::time::Duration) -> std::thread::JoinHandle<()> {
    let (held_tx, held_rx) = std::sync::mpsc::channel();
    let holder = std::thread::spawn(move || {
        let _slots = queue_timer().slots.lock();
        held_tx.send(()).unwrap();
        std::thread::sleep(hold);
    });
    held_rx.recv().unwrap();
    holder
}

fn queue_timer() -> &'static QueueTimer {
    static TIMER: std::sync::OnceLock<QueueTimer> = std::sync::OnceLock::new();
    TIMER.get_or_init(|| {
        use epics_libcom_rs::runtime::task::{MandatoryThread, StackSizeClass, ThreadPriority};
        // C: `epicsTimerQueueAllocate(1, epicsThreadPriorityScanLow)`. The
        // thread blocks in `get_or_init` until this returns.
        MandatoryThread::new(
            "asynTimerQueue",
            ThreadPriority::ScanLow,
            StackSizeClass::Medium,
        )
        .spawn(|| queue_timer().run());
        QueueTimer {
            slots: epics_libcom_rs::runtime::sync::PriorityInheritanceMutex::new(
                TimerSlots::default(),
                epics_libcom_rs::runtime::sync::CMutex::TimerQueue,
            ),
            wake: epics_libcom_rs::runtime::sync::Event::new(),
        }
    })
}

impl QueueTimer {
    fn register(&self) {
        let mut slots = self.slots.lock();
        slots.registered += 1;
        let spare = slots.registered - slots.armed.len();
        slots.armed.reserve(spare);
    }

    fn unregister(&self) {
        self.slots.lock().registered -= 1;
    }

    /// C `epicsTimerStartDelay`: one timer per cell, so a re-arm replaces it.
    fn arm(&self, cell: &Arc<RequestCell>, seq: u64, at: std::time::Instant) {
        let wake = {
            let mut slots = self.slots.lock();
            let entry = Armed {
                at,
                seq,
                cell: Arc::clone(cell),
            };
            match slots.armed.iter_mut().find(|a| Arc::ptr_eq(&a.cell, cell)) {
                Some(slot) => *slot = entry,
                None => slots.armed.push(entry),
            }
            slots.parked_until.is_none_or(|parked| at < parked)
        };
        if wake {
            self.wake.signal();
        }
    }

    /// C `epicsTimerCancel`.
    fn disarm(&self, cell: &RequestCell) {
        let gone = {
            let mut slots = self.slots.lock();
            let at = slots
                .armed
                .iter()
                .position(|a| std::ptr::eq(Arc::as_ptr(&a.cell), cell));
            at.map(|at| slots.armed.swap_remove(at))
        };
        // The cell's last reference may be this one; it drops unlocked.
        drop(gone);
    }

    fn run(&self) {
        let waiter = self.wake.waiter();
        loop {
            let now = std::time::Instant::now();
            let (due, earliest) = {
                let mut slots = self.slots.lock();
                match slots.armed.iter().position(|a| a.at <= now) {
                    Some(at) => (Some(slots.armed.swap_remove(at)), None),
                    None => {
                        let earliest = slots.armed.iter().map(|a| a.at).min();
                        slots.parked_until = earliest;
                        (None, earliest)
                    }
                }
            };
            if let Some(due) = due {
                due.cell.expire(due.seq);
                continue;
            }
            let nearer = || {
                let slots = self.slots.lock();
                slots.armed.iter().map(|a| a.at).min() != earliest
            };
            match earliest {
                Some(at) => {
                    waiter.wait_until_deadline(at, nearer);
                }
                None => waiter.wait_until(nearer),
            }
            self.slots.lock().parked_until = None;
        }
    }
}

/// A param value to set directly in the store (no writeInt32/on_param_change).
/// Mirrors C ADCore's setIntegerParam/setDoubleParam.
///
/// The value is a [`ParamValue`], not one variant per type: this carrier used to
/// enumerate its own subset of the parameter types (Int32/Float64/Octet/
/// Int32Array/Float64Array/UInt32Digital), so a driver thread pushing any other
/// supported type — `Int64`, `Int8Array`, `Int16Array`, `Int64Array`,
/// `Float32Array`, `Enum`, `GenericPointer` — simply had no variant to put it in.
/// Carrying the store's own value type means the two type sets cannot drift
/// apart again: the actor applies it through the single `ParamList::set_value`
/// dispatch, whose exhaustive match makes a new `ParamValue` variant a compile
/// error rather than an update that never arrives.
#[derive(Debug, Clone)]
pub enum ParamSetValue {
    /// Set a parameter of any type (C `setIntegerParam` / `setDoubleParam` /
    /// `setStringParam` / `doCallbacksXxxArray` …).
    Value {
        reason: usize,
        addr: i32,
        value: ParamValue,
    },
    /// asynUInt32Digital masked set — C `setUIntDigitalParam(reason, value,
    /// mask, interruptMask)`, whose write mask and forced-callback mask a plain
    /// [`Self::Value`] has no room for.
    UInt32Digital {
        reason: usize,
        addr: i32,
        value: u32,
        mask: u32,
        /// Bits to force into the I/O Intr callback mask even when the
        /// stored value is unchanged (C `setUIntDigitalParam(..,
        /// interruptMask)`); `0` for a plain value set.
        interrupt_mask: u32,
    },
    /// C `setParamStatus` from a background thread — the alarm-push half of
    /// the C pattern `lock(); setParamStatus(..); callParamCallbacks();
    /// unlock()`. The value is untouched; a status transition alone marks
    /// the param changed so the flush delivers it (paramVal.cpp:71-78), and
    /// the devEpics fill-in maps a non-Success `status` to the EPICS alarm
    /// (asynDisconnected → COMM/INVALID) while the alarm pair is 0
    /// (asynEpicsUtils.c:238-265).
    Status {
        reason: usize,
        addr: i32,
        status: AsynStatus,
    },
    /// C `setParamAlarmStatus` (paramVal.cpp:84-91).
    AlarmStatus {
        reason: usize,
        addr: i32,
        alarm_status: u16,
    },
    /// C `setParamAlarmSeverity` (paramVal.cpp:97-104).
    AlarmSeverity {
        reason: usize,
        addr: i32,
        alarm_severity: u16,
    },
}

impl ParamSetValue {
    /// Set `reason` at `addr` to `value` — any parameter type.
    pub fn new(reason: usize, addr: i32, value: ParamValue) -> Self {
        Self::Value {
            reason,
            addr,
            value,
        }
    }

    /// C `setUIntDigitalParam`: store `value & mask`, and force `interrupt_mask`
    /// bits into the callback mask even when the stored value is unchanged.
    pub fn uint32_digital(
        reason: usize,
        addr: i32,
        value: u32,
        mask: u32,
        interrupt_mask: u32,
    ) -> Self {
        Self::UInt32Digital {
            reason,
            addr,
            value,
            mask,
            interrupt_mask,
        }
    }

    /// C `setParamStatus(list, index, status)`: the transport status alone,
    /// value untouched.
    pub fn status(reason: usize, addr: i32, status: AsynStatus) -> Self {
        Self::Status {
            reason,
            addr,
            status,
        }
    }

    /// C `setParamAlarmStatus(list, index, alarmStatus)`.
    pub fn alarm_status(reason: usize, addr: i32, alarm_status: u16) -> Self {
        Self::AlarmStatus {
            reason,
            addr,
            alarm_status,
        }
    }

    /// C `setParamAlarmSeverity(list, index, alarmSeverity)`.
    pub fn alarm_severity(reason: usize, addr: i32, alarm_severity: u16) -> Self {
        Self::AlarmSeverity {
            reason,
            addr,
            alarm_severity,
        }
    }

    /// The address list this update lands in — the list whose changed flags a
    /// later `callParamCallbacks` has to consume for it to reach a record.
    pub fn addr(&self) -> i32 {
        match self {
            Self::Value { addr, .. }
            | Self::UInt32Digital { addr, .. }
            | Self::Status { addr, .. }
            | Self::AlarmStatus { addr, .. }
            | Self::AlarmSeverity { addr, .. } => *addr,
        }
    }
}

/// The octet read ops' proof of origin: only asyn-rs builds one, so an octet
/// read reaches a port only through [`OctetReadOp`] and the entry points that
/// answer it with an [`OctetRead`]. A generic `submit` replies with
/// `AsynResult<RequestResult>`, where a `?` on a timeout would drop the bytes C
/// hands back beside it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReadEntry(());

impl ReadEntry {
    pub(crate) fn new() -> Self {
        Self(())
    }
}

/// An octet read as a caller asks for it — C `asynOctet::read`, asynRecord's
/// binary read, and `asynOctetSyncIO::writeRead`.
#[derive(Debug, Clone)]
pub enum OctetReadOp {
    Read {
        buf_size: usize,
    },
    /// See [`RequestOp::OctetReadBinary`].
    ReadBinary {
        buf_size: usize,
    },
    /// `asynOctetSyncIO::writeRead`: flush, write, read, as one request
    /// ([`RequestOp::OctetWriteRead`] with `flush`).
    WriteRead {
        data: Vec<u8>,
        buf_size: usize,
    },
}

impl OctetReadOp {
    /// Crate-only: a submitted octet read op is answered only through
    /// [`octet_outcome`].
    pub(crate) fn into_op(self) -> RequestOp {
        let entry = ReadEntry(());
        match self {
            OctetReadOp::Read { buf_size } => RequestOp::OctetRead { buf_size, entry },
            OctetReadOp::ReadBinary { buf_size } => RequestOp::OctetReadBinary { buf_size, entry },
            OctetReadOp::WriteRead { data, buf_size } => RequestOp::OctetWriteRead {
                data,
                buf_size,
                flush: true,
                entry,
            },
        }
    }
}

/// C `asynOctet::read`'s whole outcome: the bytes and the eom reason it writes
/// out, and the status it returns beside them. C gives `*nbytesTransferred`
/// whatever the status (asynInterposeEos.c:255-257) and drivers publish those
/// bytes on a timeout (mar345.cpp:262-268), so the bytes come only out of
/// [`Self::into_parts`], next to the status.
#[must_use = "an octet read hands back its bytes even when it fails"]
#[derive(Debug)]
pub struct OctetRead {
    data: Vec<u8>,
    eom: crate::interpose::EomReason,
    taken_back: usize,
    status: AsynResult<()>,
}

impl OctetRead {
    pub fn new(data: Vec<u8>, eom: crate::interpose::EomReason, status: AsynResult<()>) -> Self {
        Self {
            data,
            eom,
            taken_back: 0,
            status,
        }
    }

    /// A read that moved nothing: it never ran, or failed before the read.
    pub fn failed(e: AsynError) -> Self {
        Self::new(Vec::new(), crate::interpose::EomReason::empty(), Err(e))
    }

    /// The chain's outcome, `buf` cut to the bytes it transferred.
    pub(crate) fn from_chain(mut buf: Vec<u8>, r: crate::interpose::OctetReadResult) -> Self {
        buf.truncate(r.nbytes_transferred);
        Self {
            data: buf,
            eom: r.eom_reason,
            taken_back: r.taken_back,
            status: r.status,
        }
    }

    pub(crate) fn data(&self) -> &[u8] {
        &self.data
    }

    pub fn status(&self) -> &AsynResult<()> {
        &self.status
    }

    /// The bytes and the status, together.
    pub fn into_parts(self) -> (Vec<u8>, AsynResult<()>) {
        (self.data, self.status)
    }

    pub fn eom(&self) -> crate::interpose::EomReason {
        self.eom
    }

    /// See [`crate::interpose::OctetReadResult::taken_back`].
    pub fn taken_back(&self) -> usize {
        self.taken_back
    }

    /// C `*nbytesTransferred`, before [`Self::taken_back`].
    pub fn nbytes_transferred(&self) -> usize {
        self.data.len()
    }

    /// This read, failed with `e` — the bytes, eom and take-back it made kept.
    pub(crate) fn failing(mut self, e: AsynError) -> Self {
        self.status = Err(e);
        self
    }

    /// Tag a failure with the octet step it came from.
    pub(crate) fn in_octet_step(mut self, step: crate::error::OctetStep) -> Self {
        self.status = self.status.map_err(|e| e.in_octet_step(step));
        self
    }
}

/// The [`OctetRead`] a submitted octet read op answered: the actor's, or an
/// empty failure for a request that never reached the read. Only devAsynOctet's
/// unflushed write-read skips the read (on a short write), and it reads the
/// result itself.
pub(crate) fn octet_outcome(ran: AsynResult<RequestResult>) -> OctetRead {
    match ran {
        Ok(RequestResult {
            octet: Some(read), ..
        }) => read,
        Ok(_) => OctetRead::failed(AsynError::Status {
            status: AsynStatus::Error,
            message: "octet request made no read".into(),
        }),
        Err(e) => OctetRead::failed(e),
    }
}

/// Operation the worker thread will dispatch to the port driver.
#[derive(Debug, Clone)]
pub enum RequestOp {
    OctetWrite {
        data: Vec<u8>,
    },
    OctetRead {
        buf_size: usize,
        entry: ReadEntry,
    },
    OctetWriteRead {
        data: Vec<u8>,
        buf_size: usize,
        entry: ReadEntry,
        /// Whether to drain the driver's input buffer before the write.
        /// `true` = `asynOctetSyncIO::writeRead` (flush → write → read), the
        /// StreamDevice/asynRecord pattern that discards stale warm-line bytes.
        /// `false` = `devAsynOctet` raw write-then-read (no flush) — the
        /// command-response dset (`callbackSiCmdResponse`) returns whatever the
        /// device sends, including bytes already in the buffer.
        flush: bool,
    },
    /// Binary octet write: writes `data` raw with the driver's output EOS
    /// temporarily suppressed. C parity: asynRecord binary output
    /// (`asynRecord.c:1528-1541`) saves the current output EOS, sets it to
    /// NULL for the write, and restores it. The actor performs the
    /// save/clear/restore atomically under its serial ownership so the EOS
    /// is restored on every exit path.
    OctetWriteBinary {
        data: Vec<u8>,
    },
    /// Binary octet read: reads with the driver's input EOS temporarily
    /// suppressed. C parity: asynRecord binary input
    /// (`asynRecord.c:1564-1577`) saves the current input EOS, sets it to
    /// NULL for the read, and restores it. The actor brackets the read so
    /// the EOS is restored on every exit path.
    OctetReadBinary {
        buf_size: usize,
        entry: ReadEntry,
    },
    Int32Write {
        value: i32,
    },
    Int32Read,
    Int64Write {
        value: i64,
    },
    Int64Read,
    Float64Write {
        value: f64,
    },
    Float64Read,
    UInt32DigitalWrite {
        value: u32,
        mask: u32,
    },
    UInt32DigitalRead {
        mask: u32,
    },
    Flush,
    /// A queued request that performs no transfer — C `asynCallbackProcess`
    /// (asynRecord.c:808-831) reached with `tmod == asynTMOD_NoIO` and no
    /// pending UCMD/ACMD: it resets ERRS, sets the user's timeout, and calls
    /// nothing.
    ///
    /// The point is the queue entry itself. `process()` queues without consulting
    /// TMOD (`:342-353`), so a NoI/O cycle still wakes the port thread — as
    /// the reconnect-nudge idiom pokes auto-connect — and still meets the queue
    /// gate, so a disconnected port answers it with the refusal the record turns
    /// into ERRS and STATE/MINOR.
    NoIo,
    /// Connect to the port (bypass enabled/connected checks).
    Connect,
    /// Disconnect from the port (bypass enabled/connected checks).
    Disconnect,
    /// Permanently shut down a `ASYN_DESTRUCTIBLE` port. C parity:
    /// `asynManager.c::shutdownPort` (lines 2251-2308). Marks the
    /// port defunct so every subsequent request short-circuits;
    /// idempotent; broadcasts `AsynException::Shutdown`.
    ShutdownPort,
    /// Connect a specific device address (multi-device ports).
    ConnectAddr,
    /// Disconnect a specific device address (multi-device ports).
    DisconnectAddr,
    /// Bind a user to the device its addr names — the port-side half of C
    /// `pasynManager->connectDevice` (asynManager.c:1349-1352), which is
    /// `locateDevice(pport, addr, TRUE)`: on an `ASYN_MULTIDEVICE` port the
    /// device's dpCommon is created at connect time, so it exists for
    /// `asynReport`'s `nDevices` count and per-device block before anything
    /// I/Os on it. A direct manager call under `asynManagerLock`: no queue,
    /// no enabled/connected gate.
    ConnectUser,
    /// Enable / disable — C `pasynManager->enable(pasynUser, enable)`
    /// (`asynManager.c::enable` :2224-2251, fired by asynRecord `ENBL` writes
    /// at `asynRecord.c:484-486`).
    ///
    /// One op for both scopes, as C has one call: `findDpCommon(puserPvt)`
    /// (:538-545) hands `enable` the DEVICE's `dpCommon` when the port is
    /// multi-device and the user names an address, the PORT's otherwise. The
    /// addr rides on the request's own [`crate::user::AsynUser`], so the
    /// resolution happens once, at the owner.
    SetEnable {
        yes: bool,
    },
    /// C `pasynManager->exceptionDisconnect(pasynUser)` (asynManager.c:2165-2190):
    /// mark the user's `dpCommon` down and announce it, without calling the
    /// driver's `disconnect`. A layered driver calls it on its own port when the
    /// port under it drops (drvLove.c:663-687).
    ExceptionDisconnect,
    /// C `pasynManager->exceptionConnect(pasynUser)` (asynManager.c:2142-2163):
    /// mark the user's `dpCommon` up and announce it, without calling the
    /// driver's `connect`.
    ExceptionConnect,
    /// Enable / disable auto-connect — C
    /// `pasynManager->autoConnect(pasynUser, autoConnect)`
    /// (`asynManager.c::autoConnectAsyn` :2312-2329, fired by asynRecord `AUCT`
    /// writes at `asynRecord.c:481-482`). `asynExceptionAutoConnect` is emitted
    /// unconditionally on every call. Same one-op-two-scopes resolution as
    /// [`RequestOp::SetEnable`].
    SetAutoConnect {
        yes: bool,
    },
    /// Query int32 bounds (low, high).
    GetBoundsInt32,
    /// Query int64 bounds (low, high).
    GetBoundsInt64,
    /// Query whether the port is currently enabled. C parity:
    /// `pasynManager->isEnabled` (`asynManager.c`).
    GetEnable,
    /// Query whether auto-connect is enabled for the port. C parity:
    /// `pasynManager->isAutoConnect` (`asynManager.c`).
    GetAutoConnect,
    /// C `pasynManager->blockProcessCallback(pasynUser, allDevices)`
    /// (asynManager.c:1694-1725). `all_devices` is C's own argument, and it
    /// picks which of the **two** holders this block takes: the port-wide
    /// `pport->pblockProcessHolder`, or the `pblockProcessHolder` of the one
    /// `dpCommon` the user's own `addr` resolves to (`findDpCommon`, :1766).
    /// devGpib takes the device form for an SRQ transaction
    /// (devSupportGpib.c:1216), which must not stall the port's other
    /// addresses.
    BlockProcess {
        all_devices: bool,
    },
    /// C `pasynManager->unblockProcessCallback(pasynUser, allDevices)`
    /// (asynManager.c:1727-1776) — releases the holder at the same scope.
    UnblockProcess {
        all_devices: bool,
    },
    /// Resolve a record's bind request to a parameter reason index. Carries the
    /// full [`DrvUserRequest`](crate::port::DrvUserRequest) — drvInfo, asyn `addr`, and the record's asyn
    /// interface — so an on-demand driver can create the parameter with the type
    /// the record will read it as.
    DrvUserCreate(crate::port::DrvUserRequest),
    /// Read an enum value (index + string choices).
    EnumRead,
    /// Write an enum index.
    EnumWrite {
        index: usize,
    },
    /// Read an i32 array.
    Int32ArrayRead {
        /// The caller's buffer, read in place as C passes the record's
        /// `bptr` (devAsynXXXArray.cpp:352): the driver sees its contents.
        buf: SharedArray<i32>,
        max_elements: usize,
    },
    /// Write an i32 array.
    Int32ArrayWrite {
        data: Vec<i32>,
    },
    /// Read an f64 array.
    Float64ArrayRead {
        /// The caller's buffer, read in place as C passes the record's
        /// `bptr` (devAsynXXXArray.cpp:352): the driver sees its contents.
        buf: SharedArray<f64>,
        max_elements: usize,
    },
    /// Write an f64 array.
    Float64ArrayWrite {
        data: Vec<f64>,
    },
    /// Read an i8 array.
    Int8ArrayRead {
        /// The caller's buffer, read in place as C passes the record's
        /// `bptr` (devAsynXXXArray.cpp:352): the driver sees its contents.
        buf: SharedArray<i8>,
        max_elements: usize,
    },
    /// Write an i8 array.
    Int8ArrayWrite {
        data: Vec<i8>,
    },
    /// Read an i16 array.
    Int16ArrayRead {
        /// The caller's buffer, read in place as C passes the record's
        /// `bptr` (devAsynXXXArray.cpp:352): the driver sees its contents.
        buf: SharedArray<i16>,
        max_elements: usize,
    },
    /// Write an i16 array.
    Int16ArrayWrite {
        data: Vec<i16>,
    },
    /// Read an i64 array.
    Int64ArrayRead {
        /// The caller's buffer, read in place as C passes the record's
        /// `bptr` (devAsynXXXArray.cpp:352): the driver sees its contents.
        buf: SharedArray<i64>,
        max_elements: usize,
    },
    /// Write an i64 array.
    Int64ArrayWrite {
        data: Vec<i64>,
    },
    /// Read an f32 array.
    Float32ArrayRead {
        /// The caller's buffer, read in place as C passes the record's
        /// `bptr` (devAsynXXXArray.cpp:352): the driver sees its contents.
        buf: SharedArray<f32>,
        max_elements: usize,
    },
    /// Write an f32 array.
    Float32ArrayWrite {
        data: Vec<f32>,
    },
    /// Set params directly in the store (like C setIntegerParam/setDoubleParam)
    /// and then fire interrupt notifications (callParamCallbacks).
    /// Does NOT trigger writeInt32/on_param_change — avoids re-entrancy.
    CallParamCallbacks {
        addr: i32,
        /// Param updates to apply before firing callbacks.
        /// Empty = just fire callbacks for previously changed params.
        updates: Vec<ParamSetValue>,
        /// C `setTimeStamp` ahead of the sets, as NDPluginDriver's
        /// `beginProcessCallbacks` stamps the port with the frame's epicsTS
        /// (NDPluginDriver.cpp:232); `None` keeps the port's timestamp.
        time_stamp: Option<std::time::SystemTime>,
    },
    /// C `setIntegerParam`/`setDoubleParam` alone: the values land in the
    /// store and are marked changed, and only a later `CallParamCallbacks`
    /// on their list posts them (asynPortDriver.cpp:1785-1794).
    SetParams {
        updates: Vec<ParamSetValue>,
    },
    /// Get a port/driver option by key.
    GetOption {
        key: String,
    },
    /// Set a port/driver option by key.
    SetOption {
        key: String,
        value: String,
    },
    /// Print a driver report (matches C `asynManager->report` /
    /// iocsh `asynReport`). The actor calls
    /// [`crate::port::PortDriver::report`] which writes to stderr
    /// at the requested verbosity. Carried by the actor so the
    /// driver is observed from its own thread (consistent with C
    /// asyn's `pport->lock` invariant for `report`).
    Report {
        level: i32,
        /// C's `synchronousLock` try-lock (asynManager.c:1053). `PortHandle`
        /// stamps it as the request is sent; a caller's value is replaced.
        sync_locked: bool,
    },
    /// Run caller code against the driver from the actor — what C driver
    /// threads do between `lock()` and `unlock()` (asynPortDriver.cpp:3858).
    /// Built by [`crate::port_handle::PortHandle::with_driver`].
    WithDriver(DriverCall),
    /// C `pasynManager->lockPort` … `unlockPort` (asynManager.c:1778-1808): the
    /// closure calls the port's interfaces with no `queueRequest`, so neither
    /// the enabled nor the connected check runs and no autoconnect is tried —
    /// a down link answers with the driver's own error. Only a shut-down port
    /// refuses. Built by [`crate::port_handle::PortHandle::lock_port_blocking`].
    LockPort(PortLockCall),
    /// A preallocated request the port thread runs — C `queueRequest` of a
    /// user that carries its own callbacks; see [`RequestCell`].
    Cell(Arc<RequestCell>),
    /// The same cell on a port without `ASYN_CANBLOCK`: C `queueRequest`'s
    /// synchronous branch, its refusals and then `processUser` under
    /// `synchronousLock` (asynManager.c:1555-1584), as one step of the actor,
    /// whose serial ownership of the driver is that lock. A waiter, not a
    /// queue entry. Built by [`crate::port_handle::PortHandle::queue_request`].
    CellInline(Arc<RequestCell>),
    /// Set the port's input EOS bytes — C `pasynOctet->setInputEos`.
    /// Drives the same `PortDriver::set_input_eos(&[u8])` hook the EOS
    /// interpose layer reads, so asynRecord IEOS writes survive a
    /// round trip through the actor (previously routed through the
    /// generic option HashMap which no driver consumes).
    SetInputEos {
        eos: Vec<u8>,
    },
    /// Set the port's output EOS bytes — C `pasynOctet->setOutputEos`.
    SetOutputEos {
        eos: Vec<u8>,
    },
    /// Read back the port's input EOS bytes — C `pasynOctet->getInputEos`.
    /// asynRecord's `getEos` (asynRecord.c:1987-2025) calls it after every
    /// IEOS/OEOS put so the record shows what the driver actually holds, not
    /// what was requested. Returns the bytes in [`RequestResult::data`].
    GetInputEos,
    /// Read back the port's output EOS bytes — C `pasynOctet->getOutputEos`.
    GetOutputEos,
    /// Query whether the port's *transport* is connected. C parity:
    /// `pasynManager->isConnected` — the state the driver publishes through
    /// `exceptionConnect`/`exceptionDisconnect`, not "is a record bound to
    /// this port". `asynRecord` reads it in `monitorStatus` (asynRecord.c:
    /// 1090-1094) to refresh CNCT, and gates its `callbackConnect` on it
    /// (:858-888) so a CNCT put never re-connects an already-connected port.
    GetConnected,
    /// Install the echo interpose on top of the port's octet stack. C parity:
    /// `asynInterposeEcho(portName, addr)`
    /// (`asynInterposeEcho.c:165-186`), the iocsh command a startup script
    /// runs *after* the port is configured.
    ///
    /// It is a request rather than a direct `install_interpose` because the actor
    /// owns the driver once the port is registered — the same reason
    /// `SetOption` / `SetInputEos` are requests. Installing from the shell
    /// thread would race every in-flight transfer.
    PushEchoInterpose,
    /// Install the delay interpose on top of the port's octet stack. C parity:
    /// `asynInterposeDelay(portName, addr, delay)`
    /// (`asynInterposeDelay.c:176-215`), registered with iocsh at
    /// `asynInterposeDelay.c:215-237`. Same actor-ownership reason as
    /// [`RequestOp::PushEchoInterpose`].
    PushDelayInterpose {
        delay: std::time::Duration,
    },
    /// Install the EOS interpose on the addressed device's octet stack. C
    /// parity: `asynInterposeEosConfig(portName, addr, processEosIn,
    /// processEosOut)` (`asynInterposeEos.c:84-140`), registered with iocsh at
    /// :394-411. The two flags select which half of the layer is live.
    PushEosInterpose {
        process_in: bool,
        process_out: bool,
    },
    /// Set (or clear) the port's time-stamp source by NAME. C parity:
    /// `asynRegisterTimeStampSource(portName, functionName)` /
    /// `asynUnregisterTimeStampSource(portName)` (asynShellCommands.c:1181-1223)
    /// — C resolves the name through `registryFunctionFind` and hands the
    /// function to `pasynManager->registerTimeStampSource`. The NAME travels,
    /// not the function: that is what makes it resolvable on the far side of a
    /// remote port, exactly as C resolves it in the IOC's own registry.
    /// `None` = unregister (back to the driver's default clock).
    SetTimeStampSource {
        name: Option<String>,
    },
    /// Install the flush-timeout interpose on the addressed device's octet
    /// stack. C parity: `asynInterposeFlushConfig(portName, addr, timeout)`
    /// (`asynInterposeFlush.c:66-91`); C's shell argument is in milliseconds
    /// and `<= 0` means 1 ms (:78-79), so the conversion happens at the shell
    /// and the op carries a real duration.
    PushFlushInterpose {
        flush_timeout: std::time::Duration,
    },
    /// Send a GPIB universal command byte — C `asynGpib::universalCmd`
    /// (asynGpib.c:480-484). asynRecord's UCMD dispatch
    /// (`gpibUniversalCmd`, asynRecord.c:1638-1679).
    GpibUniversalCmd {
        cmd: u8,
    },
    /// Send a GPIB addressed-command frame — C `asynGpib::addressedCmd`
    /// (asynGpib.c:472-478). asynRecord's ACMD dispatch builds the frame
    /// (`gpibAddressedCmd`, asynRecord.c:1681-1756).
    GpibAddressedCmd {
        data: Vec<u8>,
    },
    /// Assert Interface Clear — C `asynGpib::ifc` (asynGpib.c:486-490).
    GpibIfc,
    /// Set the Remote Enable line — C `asynGpib::ren` (asynGpib.c:492-496).
    GpibRen {
        enable: bool,
    },
}

/// Result returned by the worker after executing a request.
#[derive(Debug)]
pub struct RequestResult {
    pub status: AsynStatus,
    pub message: String,
    /// Bytes an octet write moved: the write, or a write-read's write half.
    pub nbytes: usize,
    pub data: Option<Vec<u8>>,
    /// An octet read op's outcome, bytes and status together. `None` on an
    /// unflushed write-read whose write came up short, which reads nothing.
    pub(crate) octet: Option<OctetRead>,
    pub int_val: Option<i32>,
    pub int64_val: Option<i64>,
    pub float_val: Option<f64>,
    pub uint_val: Option<u32>,
    /// Reason index (from DrvUserCreate).
    pub reason: Option<usize>,
    /// Per-record octet length cap (from DrvUserCreate; C `modbusDrvUser_t.len`).
    /// `None` when the drvInfo carried no cap.
    pub max_octet_len: Option<usize>,
    /// The drvUser (from DrvUserCreate).
    pub drv_user: Option<crate::port::DrvUser>,
    /// Enum index (from EnumRead).
    pub enum_index: Option<usize>,
    /// Driver enum string/value/severity table (from EnumRead). C asyn
    /// device support reads this via `asynEnum->read` and pushes it onto
    /// the record's state fields (ZRST/ZRVL/ZRSV…, ZNAM/ONAM…) at init —
    /// see `devAsynInt32.c::initCommon` (298-324) / `setEnums` (415-435).
    pub enum_entries: Option<Arc<[crate::param::EnumEntry]>>,
    /// i32 array data (from Int32ArrayRead).
    pub int32_array: Option<SharedArray<i32>>,
    /// f64 array data (from Float64ArrayRead).
    pub float64_array: Option<SharedArray<f64>>,
    /// i8 array data (from Int8ArrayRead).
    pub int8_array: Option<SharedArray<i8>>,
    /// i16 array data (from Int16ArrayRead).
    pub int16_array: Option<SharedArray<i16>>,
    /// i64 array data (from Int64ArrayRead).
    pub int64_array: Option<SharedArray<i64>>,
    /// f32 array data (from Float32ArrayRead).
    pub float32_array: Option<SharedArray<f32>>,
    /// Alarm status from the driver param store (populated on reads).
    pub alarm_status: u16,
    /// Alarm severity from the driver param store (populated on reads).
    pub alarm_severity: u16,
    /// Timestamp from the driver param store (populated on reads).
    pub timestamp: Option<SystemTime>,
    /// Option value string (from GetOption).
    pub option_value: Option<String>,
    /// Int64 bounds (from GetBoundsInt32/Int64).
    pub bounds: Option<(i64, i64)>,
    /// The bound `dpCommon`'s cells (from ConnectUser).
    pub dp: Option<crate::port::DpCells>,
}

impl RequestResult {
    fn base() -> Self {
        Self {
            status: AsynStatus::Success,
            message: String::new(),
            nbytes: 0,
            data: None,
            octet: None,
            int_val: None,
            int64_val: None,
            float_val: None,
            uint_val: None,
            reason: None,
            max_octet_len: None,
            drv_user: None,
            enum_index: None,
            enum_entries: None,
            int32_array: None,
            float64_array: None,
            int8_array: None,
            int16_array: None,
            int64_array: None,
            float32_array: None,
            alarm_status: 0,
            alarm_severity: 0,
            timestamp: None,
            option_value: None,
            bounds: None,
            dp: None,
        }
    }

    pub fn write_ok() -> Self {
        Self::base()
    }

    /// Octet write result carrying the number of bytes transferred
    /// (C `asynOctet::write`'s `*nbytesTransferred`). Used by
    /// `PortHandle::write_octet` / `SyncIO::write_octet` to report how
    /// many bytes the driver actually wrote on success.
    pub fn write_n(nbytes: usize) -> Self {
        Self {
            nbytes,
            ..Self::base()
        }
    }

    /// An octet read op's reply: `nbytes` is its write half's count.
    pub fn octet_read(read: OctetRead, nbytes: usize) -> Self {
        Self {
            nbytes,
            octet: Some(read),
            ..Self::base()
        }
    }

    /// A `GetInputEos`/`GetOutputEos` reply: the terminator in `data`.
    pub fn eos_read(eos: Vec<u8>) -> Self {
        Self {
            nbytes: eos.len(),
            data: Some(eos),
            ..Self::base()
        }
    }

    /// The octet read a test's reply must carry.
    #[cfg(test)]
    pub(crate) fn into_read(self) -> OctetRead {
        self.octet.expect("the reply carries an octet read")
    }

    /// The failure of the octet read this reply carries.
    pub fn octet_error(&self) -> Option<&AsynError> {
        self.octet.as_ref()?.status().as_ref().err()
    }

    /// The octet read this reply carries, if the op made one.
    pub fn octet(&self) -> Option<&OctetRead> {
        self.octet.as_ref()
    }

    pub fn int32_read(value: i32) -> Self {
        Self {
            int_val: Some(value),
            ..Self::base()
        }
    }

    pub fn int64_read(value: i64) -> Self {
        Self {
            int64_val: Some(value),
            ..Self::base()
        }
    }

    pub fn float64_read(value: f64) -> Self {
        Self {
            float_val: Some(value),
            ..Self::base()
        }
    }

    pub fn uint32_read(value: u32) -> Self {
        Self {
            uint_val: Some(value),
            ..Self::base()
        }
    }

    pub fn drv_user_create(info: crate::port::DrvUserInfo) -> Self {
        Self {
            reason: Some(info.reason),
            max_octet_len: info.max_octet_len,
            drv_user: info.drv_user,
            ..Self::base()
        }
    }

    pub fn enum_read(index: usize) -> Self {
        Self {
            enum_index: Some(index),
            ..Self::base()
        }
    }

    /// [`Self::enum_read`] carrying the driver's full enum table so the
    /// device-support init path can propagate it to the record's state
    /// fields (C `setEnums`). The index is the current selection.
    pub fn enum_read_with_entries(index: usize, entries: Arc<[crate::param::EnumEntry]>) -> Self {
        Self {
            enum_index: Some(index),
            enum_entries: Some(entries),
            ..Self::base()
        }
    }

    pub fn int32_array_read(data: impl Into<SharedArray<i32>>) -> Self {
        Self {
            int32_array: Some(data.into()),
            ..Self::base()
        }
    }

    pub fn float64_array_read(data: impl Into<SharedArray<f64>>) -> Self {
        Self {
            float64_array: Some(data.into()),
            ..Self::base()
        }
    }

    pub fn int8_array_read(data: impl Into<SharedArray<i8>>) -> Self {
        Self {
            int8_array: Some(data.into()),
            ..Self::base()
        }
    }

    pub fn int16_array_read(data: impl Into<SharedArray<i16>>) -> Self {
        Self {
            int16_array: Some(data.into()),
            ..Self::base()
        }
    }

    pub fn int64_array_read(data: impl Into<SharedArray<i64>>) -> Self {
        Self {
            int64_array: Some(data.into()),
            ..Self::base()
        }
    }

    pub fn float32_array_read(data: impl Into<SharedArray<f32>>) -> Self {
        Self {
            float32_array: Some(data.into()),
            ..Self::base()
        }
    }

    pub fn option_read(value: String) -> Self {
        Self {
            option_value: Some(value),
            ..Self::base()
        }
    }

    pub fn bounds_read(low: i64, high: i64) -> Self {
        Self {
            bounds: Some((low, high)),
            ..Self::base()
        }
    }

    /// Attach alarm/timestamp metadata to this result.
    pub fn with_alarm(
        mut self,
        alarm_status: u16,
        alarm_severity: u16,
        timestamp: Option<SystemTime>,
    ) -> Self {
        self.alarm_status = alarm_status;
        self.alarm_severity = alarm_severity;
        self.timestamp = timestamp;
        self
    }
}

/// Lifecycle of a queued request, mirroring C `asynManager` queue/callback
/// state so that `AQR` cancellation reproduces the `cancelRequest` `wasQueued`
/// split (asynManager.c:1632-1692) by construction rather than by a runtime
/// guard.
///
/// `cancelRequest` removes the request and reports `wasQueued==1` ONLY while it
/// is still on the queue (asynManager.c:1663-1668); once the port thread has
/// dequeued it and is running the callback (`callbackActive`) or it has already
/// finished, `wasQueued==0` and the I/O runs to completion and is reported
/// normally (asynManager.c:1647-1661). `Queued` is the only state a cancel can
/// win from; the executor's `Queued -> Running` transition closes that window.
///
/// The queue-wait timeout (C `queueTimeoutCallback`, asynManager.c:649-703) is
/// the second way a request can leave the queue without running, and it obeys
/// the same rule: the timer callback returns immediately when `!isQueued`
/// (:657-663), so a request the port thread has already dequeued always
/// completes. `TimedOut` is therefore a sibling of `Cancelled` — a terminal
/// state reachable only from `Queued` — and the two together are the complete
/// set of "this request never ran" outcomes. Which one won is what tells the
/// caller *which* C callback to report ("I/O request canceled" vs "process
/// queueRequest timeout"), so they are distinct states rather than one flag.
const STATE_QUEUED: u8 = 0;
const STATE_RUNNING: u8 = 1;
const STATE_DONE: u8 = 2;
const STATE_CANCELLED: u8 = 3;
const STATE_TIMED_OUT: u8 = 4;

/// Token tracking the queue/execution lifecycle of an off-thread request.
///
/// The state machine makes the C `wasQueued` semantics hold by construction:
/// `cancel()` succeeds only from `Queued`, the executor claims the request with
/// `begin_running()` (refused once cancelled) and releases it with `finish()`,
/// so a cancel that arrives after execution started cannot transition the token
/// and is a no-op — the I/O completes and applies normally.
#[derive(Clone, Debug)]
pub struct CancelToken(pub Arc<AtomicU8>);

impl CancelToken {
    pub fn new() -> Self {
        Self(Arc::new(AtomicU8::new(STATE_QUEUED)))
    }

    /// `AQR` / C `cancelRequest`: cancel the request iff it is still queued.
    ///
    /// Returns the C `wasQueued` flag — `true` when the request was removed
    /// from the queue (the caller must report "I/O request canceled",
    /// asynRecord.c:397-404); `false` when it had already been dequeued and was
    /// running or had completed, in which case the I/O runs to completion and
    /// reports normally (asynManager.c:1647-1661).
    pub fn cancel(&self) -> bool {
        self.0
            .compare_exchange(
                STATE_QUEUED,
                STATE_CANCELLED,
                AtomicOrdering::AcqRel,
                AtomicOrdering::Acquire,
            )
            .is_ok()
    }

    /// C `queueTimeoutCallback` (asynManager.c:649-703): the queue-wait deadline
    /// expired. Removes the request from the queue iff it is still queued —
    /// C's `if(!puserPvt->isQueued) { ...; return; }` guard (:657-663) — and
    /// returns whether it won.
    ///
    /// `false` means the port thread had already dequeued the request: the timer
    /// fired too late, the I/O runs to completion and reports normally, and the
    /// caller must keep waiting for it. This is the same `isQueued` gate
    /// [`Self::cancel`] answers for `AQR`, so a cancel and a timeout racing the
    /// same request cannot both win.
    pub fn time_out_if_queued(&self) -> bool {
        self.0
            .compare_exchange(
                STATE_QUEUED,
                STATE_TIMED_OUT,
                AtomicOrdering::AcqRel,
                AtomicOrdering::Acquire,
            )
            .is_ok()
    }

    /// The port thread takes the request off the queue without running it — a
    /// refusal at dequeue. `false` when it had already left.
    pub fn leave_if_queued(&self) -> bool {
        self.0
            .compare_exchange(
                STATE_QUEUED,
                STATE_DONE,
                AtomicOrdering::AcqRel,
                AtomicOrdering::Acquire,
            )
            .is_ok()
    }

    /// Executor at dequeue: claim the request for execution (C dequeue under
    /// `asynManagerLock`, asynManager.c:1663-1668 is the cancel counterpart).
    ///
    /// Returns `false` iff the request left the queue without running — it was
    /// cancelled (`AQR`) or its queue-wait deadline expired — in which case the
    /// executor must drop it and report that outcome. Otherwise the token enters
    /// `Running`. A multi-phase plan re-claims the same token for its next phase
    /// from `Done`, so this transitions from either `Queued` or `Done`;
    /// `Cancelled` and `TimedOut` are terminal.
    pub fn begin_running(&self) -> bool {
        let mut cur = self.0.load(AtomicOrdering::Acquire);
        loop {
            if cur == STATE_CANCELLED || cur == STATE_TIMED_OUT {
                return false;
            }
            match self.0.compare_exchange_weak(
                cur,
                STATE_RUNNING,
                AtomicOrdering::AcqRel,
                AtomicOrdering::Acquire,
            ) {
                Ok(_) => return true,
                Err(actual) => cur = actual,
            }
        }
    }

    /// Executor at completion: mark the running request finished so a later
    /// cancel is a no-op (the C `wasQueued==0` window). Idempotent and a no-op
    /// from any state other than `Running`.
    pub fn finish(&self) {
        let _ = self.0.compare_exchange(
            STATE_RUNNING,
            STATE_DONE,
            AtomicOrdering::AcqRel,
            AtomicOrdering::Acquire,
        );
    }

    /// A [`RequestCell`] queued again: its previous request has run or left.
    pub(crate) fn requeue(&self) {
        self.0.store(STATE_QUEUED, AtomicOrdering::Release);
    }

    /// True iff the request was cancelled while still queued — the C
    /// `wasQueued==true` outcome. A cancel that lost the race (the executor had
    /// already begun running) leaves the state `Running`/`Done`, so this stays
    /// `false` and the completed I/O applies normally.
    pub fn is_cancelled(&self) -> bool {
        self.0.load(AtomicOrdering::Acquire) == STATE_CANCELLED
    }

    /// True iff the request was removed from the queue by its queue-wait
    /// deadline — the C `queueTimeoutCallback` outcome. Mutually exclusive with
    /// [`Self::is_cancelled`]: both transition out of `Queued`, so exactly one
    /// can win.
    pub fn is_timed_out(&self) -> bool {
        self.0.load(AtomicOrdering::Acquire) == STATE_TIMED_OUT
    }
}

impl Default for CancelToken {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    /// bab72deef: the entry the actor still holds for a request the timer took
    /// (C unlinks it, asynManager.c:664-674) must not claim the cell's next
    /// request — `begin_running` from `Done` would run that one twice.
    #[test]
    fn a_timed_out_entry_does_not_claim_the_next_request() {
        struct Nothing;
        impl CellBody for Nothing {
            fn process(&self, _: &mut dyn PortCalls) {}
            fn unrun(&self, _: Unrun) {}
            fn done(self: Arc<Self>) {}
        }
        let queue = Arc::new(CellQueue::default());
        let cell = queue.new_cell(Arc::new(Nothing));
        let user = crate::user::AsynUser::new(0);
        cell.queue(&user).unwrap();
        // The actor took the entry, then the timer expired the request.
        let (_, stale) = queue.pop().unwrap();
        cell.expire(stale);
        cell.queue(&user).unwrap();
        let (_, next) = queue.pop().unwrap();
        assert!(matches!(cell.claim(stale), Claim::Gone));
        assert!(matches!(cell.claim(next), Claim::Run));
    }

    /// What a [`Probe`] saw: the callback, the cell's state inside it, and the
    /// refusal of the requeue it made there, if it made one.
    type Seen = (
        &'static str,
        CellState,
        Option<Result<(), QueueRefusal<'static>>>,
    );

    /// A body that queues its own cell again from inside its callback, and
    /// optionally claims that request there too, as the port thread could.
    #[derive(Default)]
    struct Probe {
        cell: std::sync::OnceLock<std::sync::Weak<RequestCell>>,
        requeue: bool,
        claim: std::sync::atomic::AtomicBool,
        seen: Mutex<Vec<Seen>>,
    }

    impl Probe {
        fn cell(&self) -> Arc<RequestCell> {
            self.cell.get().unwrap().upgrade().unwrap()
        }

        fn callback(&self, what: &'static str) {
            let cell = self.cell();
            let requeued = self
                .requeue
                .then(|| cell.queue(&crate::user::AsynUser::new(0)));
            if self.claim.swap(false, AtomicOrdering::Relaxed) {
                let (_, seq) = cell.queue.pop().unwrap();
                assert!(matches!(cell.claim(seq), Claim::Run));
            }
            let state = cell.lock().state;
            self.seen.lock().push((what, state, requeued));
        }

        fn seen(&self) -> Vec<Seen> {
            std::mem::take(&mut *self.seen.lock())
        }
    }

    impl CellBody for Probe {
        fn process(&self, _: &mut dyn PortCalls) {
            self.callback("process");
        }
        fn unrun(&self, why: Unrun) {
            self.callback(match why {
                Unrun::Canceled => "canceled",
                Unrun::TimedOut => "timed out",
                Unrun::Refused(_) => "refused",
            });
        }
        fn done(self: Arc<Self>) {
            let state = self.cell().lock().state;
            self.seen.lock().push(("done", state, None));
        }
    }

    struct NoPort;
    impl PortCalls for NoPort {
        fn call(&mut self, _: &RequestOp, _: crate::user::AsynUser) -> AsynResult<RequestResult> {
            unreachable!("a probe calls no port")
        }
    }

    fn probe(requeue: bool) -> (Arc<CellQueue>, Arc<Probe>, Arc<RequestCell>) {
        let queue = Arc::new(CellQueue::default());
        let body = Arc::new(Probe {
            requeue,
            ..Probe::default()
        });
        let cell = queue.new_cell(body.clone());
        body.cell.set(Arc::downgrade(&cell)).unwrap();
        (queue, body, cell)
    }

    const ACTIVE_QUEUED: CellState = CellState::ActiveQueued { callbacks: 1 };

    /// C clears `isQueued` before `timeoutUser` (asynManager.c:684, :689), so
    /// `queueRequest` from inside it is accepted (:1586-1590) — for each of
    /// the three ways a queued request leaves without running.
    #[test]
    fn a_requeue_from_inside_unrun_is_accepted() {
        fn cancel(cell: &RequestCell, _: u64) {
            assert!(cell.cancel());
        }
        fn leave(cell: &RequestCell, seq: u64) {
            // The actor took the entry and refuses it.
            assert_eq!(cell.queue.pop().unwrap().1, seq);
            let port = String::new();
            cell.leave(seq, AsynError::QueueTimeout { port });
        }
        type Exit = fn(&RequestCell, u64);
        let exits: [(&str, Exit); 3] = [
            ("canceled", cancel),
            ("timed out", RequestCell::expire),
            ("timed out", leave),
        ];
        for (what, exit) in exits {
            let (queue, body, cell) = probe(true);
            cell.queue(&crate::user::AsynUser::new(0)).unwrap();
            let seq = cell.request().seq;
            exit(&cell, seq);
            assert_eq!(
                body.seen(),
                [
                    (what, ACTIVE_QUEUED, Some(Ok(()))),
                    ("done", CellState::Queued, None)
                ],
            );
            // The left entry is unlinked, or was the actor's (`leave`); only
            // the requeued one is on the queue, and it is the one that runs.
            let (_, next) = queue.pop().unwrap();
            assert!(queue.pop().is_none());
            assert_ne!(next, seq);
            assert!(cell.is_gone(seq));
            assert!(matches!(cell.claim(next), Claim::Run));
        }
    }

    /// The refusal is `isQueued` only: queued, or queued again while a
    /// callback runs.
    #[test]
    fn a_queued_cell_refuses_a_second_request() {
        let user = crate::user::AsynUser::new(0);
        let (queue, _body, cell) = probe(false);
        cell.queue(&user).unwrap();
        assert_eq!(cell.queue(&user), Err(QueueRefusal::AlreadyQueued));
        let (_, seq) = queue.pop().unwrap();
        assert!(matches!(cell.claim(seq), Claim::Run));
        cell.queue(&user).unwrap();
        assert_eq!(cell.lock().state, ACTIVE_QUEUED);
        assert_eq!(cell.queue(&user), Err(QueueRefusal::AlreadyQueued));
    }

    /// asynManager.c:1592-1597: a queue timeout needs a timeout callback.
    #[test]
    fn a_queue_timeout_without_a_timeout_user_is_refused() {
        struct NoTimeoutUser;
        impl CellBody for NoTimeoutUser {
            fn process(&self, _: &mut dyn PortCalls) {}
            fn unrun(&self, _: Unrun) {}
            fn done(self: Arc<Self>) {}
            fn timeout_user(&self) -> bool {
                false
            }
        }
        let queue = Arc::new(CellQueue::default());
        let cell = queue.new_cell(Arc::new(NoTimeoutUser));
        let timed =
            crate::user::AsynUser::new(0).with_queue_timeout(std::time::Duration::from_secs(1));
        assert_eq!(cell.queue(&timed), Err(QueueRefusal::NoTimeoutUser));
        assert_eq!(cell.lock().state, CellState::Idle);
        cell.queue(&crate::user::AsynUser::new(0)).unwrap();
    }

    /// C `state == callbackActive` with `isQueued` clear: a cancel finds
    /// nothing queued (asynManager.c:1647-1661), the callback may queue the
    /// cell again, and `done` follows the callback's end.
    #[test]
    fn done_follows_a_callback_that_was_active() {
        let (queue, body, cell) = probe(true);
        cell.queue(&crate::user::AsynUser::new(0)).unwrap();
        let (_, seq) = queue.pop().unwrap();
        assert!(matches!(cell.claim(seq), Claim::Run));
        assert_eq!(cell.lock().state, CellState::Active { callbacks: 1 });
        assert!(
            !cell.cancel(),
            "nothing queued while the callback is active"
        );
        cell.run(&mut NoPort);
        assert_eq!(
            body.seen(),
            [
                ("process", ACTIVE_QUEUED, Some(Ok(()))),
                ("done", CellState::Queued, None)
            ],
        );
        // Without a requeue the callback's end leaves the cell idle.
        let (queue, body, cell) = probe(false);
        cell.queue(&crate::user::AsynUser::new(0)).unwrap();
        let (_, seq) = queue.pop().unwrap();
        assert!(matches!(cell.claim(seq), Claim::Run));
        cell.run(&mut NoPort);
        assert_eq!(
            body.seen(),
            [
                ("process", CellState::Active { callbacks: 1 }, None),
                ("done", CellState::Idle, None)
            ],
        );
    }

    /// The request an `unrun` queued is claimed by the port thread before
    /// that `unrun` returns: callback-active ends with the second callback.
    #[test]
    fn overlapping_callbacks_end_one_at_a_time() {
        let (_queue, body, cell) = probe(true);
        body.claim.store(true, AtomicOrdering::Relaxed);
        cell.queue(&crate::user::AsynUser::new(0)).unwrap();
        cell.expire(cell.request().seq);
        assert_eq!(
            body.seen(),
            [
                (
                    "timed out",
                    CellState::Active { callbacks: 2 },
                    Some(Ok(()))
                ),
                ("done", CellState::Active { callbacks: 1 }, None)
            ],
        );
        // The claimed request's `process` queues once more as it ends.
        cell.run(&mut NoPort);
        assert_eq!(
            body.seen(),
            [
                ("process", ACTIVE_QUEUED, Some(Ok(()))),
                ("done", CellState::Queued, None)
            ],
        );
    }

    /// A request the closed queue refused was never queued: nothing can
    /// cancel it, and a requeue from the shutdown's `unrun` is refused for the
    /// shutdown, not as already queued.
    #[test]
    fn a_shut_down_refusal_leaves_nothing_queued() {
        let user = crate::user::AsynUser::new(0);
        let (queue, body, cell) = probe(true);
        cell.queue(&user).unwrap();
        queue.close();
        assert_eq!(
            body.seen(),
            [
                (
                    "refused",
                    CellState::Active { callbacks: 1 },
                    Some(Err(QueueRefusal::ShutDown))
                ),
                ("done", CellState::Idle, None)
            ],
        );
        assert_eq!(cell.queue(&user), Err(QueueRefusal::ShutDown));
        assert_eq!(cell.lock().state, CellState::Idle);
        assert!(!cell.cancel());
        assert!(body.seen().is_empty());
    }

    use super::*;

    #[test]
    fn cancel_succeeds_only_while_queued() {
        // C `wasQueued==1`: a still-queued request is cancelled and removed.
        let token = CancelToken::new();
        assert!(!token.is_cancelled());
        assert!(token.cancel(), "a queued request reports wasQueued==true");
        assert!(token.is_cancelled());
        // The executor then refuses to run it (it was removed from the queue).
        assert!(
            !token.begin_running(),
            "a cancelled request is not claimed for execution"
        );
    }

    #[test]
    fn cancel_after_begin_running_is_noop() {
        // C `wasQueued==0` while `callbackActive`: the I/O runs to completion.
        let token = CancelToken::new();
        assert!(
            token.begin_running(),
            "the executor claims a queued request"
        );
        assert!(
            !token.cancel(),
            "a cancel during execution reports wasQueued==false"
        );
        assert!(
            !token.is_cancelled(),
            "the running I/O is not treated as cancelled"
        );
        token.finish();
        assert!(!token.is_cancelled(), "the completed I/O applies normally");
    }

    #[test]
    fn cancel_after_finish_is_noop() {
        // C `wasQueued==0` after the callback finished: nothing to cancel.
        let token = CancelToken::new();
        assert!(token.begin_running());
        token.finish();
        assert!(
            !token.cancel(),
            "a cancel after completion reports wasQueued==false"
        );
        assert!(!token.is_cancelled());
    }

    #[test]
    fn begin_running_reclaims_token_for_next_phase() {
        // A WriteRead plan threads one token through two phases; the read phase
        // re-claims the token the write phase finished.
        let token = CancelToken::new();
        assert!(token.begin_running(), "write phase claims the queued token");
        token.finish();
        assert!(
            token.begin_running(),
            "read phase re-claims the finished token"
        );
        token.finish();
        assert!(!token.is_cancelled());
    }

    #[test]
    fn cancel_is_terminal_across_phases() {
        // Once cancelled while queued, no later phase may run.
        let token = CancelToken::new();
        assert!(token.cancel());
        assert!(!token.begin_running(), "cancelled is terminal");
        assert!(token.is_cancelled());
    }

    #[test]
    fn octet_read_chain_keeps_the_taken_back_byte() {
        use crate::interpose::{EomReason, OctetReadResult};
        let r = OctetRead::from_chain(
            vec![0; 4],
            OctetReadResult {
                nbytes_transferred: 0,
                eom_reason: EomReason::EOS,
                taken_back: 1,
                status: Ok(()),
            },
        );
        assert_eq!(
            (r.nbytes_transferred(), r.taken_back(), r.eom()),
            (0, 1, EomReason::EOS)
        );
    }

    /// The bytes ride beside a failing status, so a timeout after a partial
    /// line still hands them over (asynInterposeEos.c:255-257).
    #[test]
    fn a_failed_read_keeps_its_bytes() {
        use crate::interpose::{EomReason, OctetReadResult};
        let r = OctetRead::from_chain(
            b"abcxx".to_vec(),
            OctetReadResult {
                nbytes_transferred: 3,
                eom_reason: EomReason::empty(),
                taken_back: 0,
                status: Err(AsynError::Status {
                    status: AsynStatus::Timeout,
                    message: "timeout".into(),
                }),
            },
        );
        let (data, status) = r.into_parts();
        assert_eq!(data, b"abc");
        assert_eq!(status.unwrap_err().status(), AsynStatus::Timeout);
    }
}
