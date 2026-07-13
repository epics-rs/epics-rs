//! Actor-based port driver executor.
//!
//! Each port driver is owned exclusively by a `PortActor` task. Requests arrive
//! via an mpsc channel, are prioritized in a heap, and dispatched to the
//! driver's `io_*` methods. Replies go back through oneshot channels.
//!
//! For `can_block=true` ports, the actor runs on `tokio::task::spawn_blocking`.
//! For `can_block=false` ports, it runs on a normal `tokio::spawn` task.

use std::cell::Cell;
use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use std::time::Instant;

use tokio::sync::{mpsc, oneshot};

use crate::error::{AsynError, AsynResult, AsynStatus};
use crate::port::{PortDriver, QueuePriority};
use crate::request::{CancelToken, RequestOp, RequestResult};
use crate::user::AsynUser;

static ACTOR_SEQ: AtomicU64 = AtomicU64::new(0);
static ACTOR_IDS: AtomicU64 = AtomicU64::new(0);

/// Identity of a port actor.
///
/// Minted once per [`PortActor`] and carried by every
/// [`PortHandle`](crate::port_handle::PortHandle) that targets it, so that a
/// caller about to block can answer the one question that decides whether
/// blocking is safe: *am I the thread that would have to service the request I
/// am about to wait on?*
///
/// The actor publishes its own id on its thread for as long as it runs (see
/// [`ActorScope`]); [`current_actor`] reads it back.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct ActorId(u64);

impl ActorId {
    /// Mint a fresh identity, distinct from every other actor's.
    ///
    /// A handle whose actor is never spawned (some unit-test fixtures hold the
    /// receiving end and run no actor loop) gets an id that is never published
    /// on any thread — which is the truth: no thread is that port's actor, so
    /// no caller can be it.
    pub(crate) fn new() -> Self {
        Self(ACTOR_IDS.fetch_add(1, AtomicOrdering::Relaxed))
    }
}

thread_local! {
    /// The port actor running on this thread, if this thread is an actor thread.
    static CURRENT_ACTOR: Cell<Option<ActorId>> = const { Cell::new(None) };
}

/// Publishes an actor's id on its own thread, and unpublishes it on every exit
/// path (return, panic).
struct ActorScope(Option<ActorId>);

impl ActorScope {
    fn enter(id: ActorId) -> Self {
        Self(CURRENT_ACTOR.with(|cur| cur.replace(Some(id))))
    }
}

impl Drop for ActorScope {
    fn drop(&mut self) {
        CURRENT_ACTOR.with(|cur| cur.set(self.0));
    }
}

/// The port actor owning the calling thread, or `None` if the caller is not an
/// actor thread (a device-support scan thread, an `ad-core` driver thread, a
/// plain `std::thread`, a tokio worker).
pub(crate) fn current_actor() -> Option<ActorId> {
    CURRENT_ACTOR.with(|cur| cur.get())
}

/// Message sent from [`super::port_handle::PortHandle`] to the actor.
pub(crate) struct ActorMessage {
    pub op: RequestOp,
    pub user: AsynUser,
    pub cancel: CancelToken,
    pub reply: oneshot::Sender<AsynResult<RequestResult>>,
    pub seq: u64,
    pub priority: QueuePriority,
    pub block_token: Option<u64>,
}

impl ActorMessage {
    pub fn new(
        op: RequestOp,
        user: AsynUser,
        cancel: CancelToken,
        reply: oneshot::Sender<AsynResult<RequestResult>>,
    ) -> Self {
        let priority = user.priority;
        let block_token = user.block_token;
        Self {
            op,
            user,
            cancel,
            reply,
            seq: ACTOR_SEQ.fetch_add(1, AtomicOrdering::Relaxed),
            priority,
            block_token,
        }
    }
}

/// Marks a claimed request's cancel token `Done` when execution leaves the
/// dispatch scope, on every exit path (reply, early return, panic). This is the
/// C `callbackActive -> idle` transition: once the port thread finishes the
/// callback, a pending `cancelRequest` reports `wasQueued==0` (asynManager.c:1645-1659).
struct FinishGuard(CancelToken);

impl Drop for FinishGuard {
    fn drop(&mut self) {
        self.0.finish();
    }
}

// Heap ordering: higher priority first, then lower seq (strict FIFO
// within a priority). C asynManager queues each priority as a FIFO list
// (queueRequest ellAdd to the queueList[priority] tail; portThread walks
// ellFirst->ellNext, asynManager.c:1612-1613/869-898) — a request's
// timeout never reorders it relative to same-priority peers. An earlier
// deadline-based tiebreaker let a later-submitted request with a shorter
// timeout jump ahead of an earlier one, violating that FIFO.
impl Eq for ActorMessage {}
impl PartialEq for ActorMessage {
    fn eq(&self, other: &Self) -> bool {
        self.seq == other.seq
    }
}
impl Ord for ActorMessage {
    fn cmp(&self, other: &Self) -> Ordering {
        self.priority
            .cmp(&other.priority)
            .then_with(|| other.seq.cmp(&self.seq))
    }
}
impl PartialOrd for ActorMessage {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// The actor that exclusively owns a port driver instance.
pub(crate) struct PortActor {
    id: ActorId,
    driver: Box<dyn PortDriver>,
    rx: mpsc::Receiver<ActorMessage>,
    heap: BinaryHeap<ActorMessage>,
    /// (token, nesting_count) — C parity: blockPortCount with nested lock support.
    blocked_by: Option<(u64, u32)>,
    pending_while_blocked: Vec<ActorMessage>,
}

impl PortActor {
    pub fn new(driver: Box<dyn PortDriver>, rx: mpsc::Receiver<ActorMessage>) -> Self {
        Self {
            id: ActorId::new(),
            driver,
            rx,
            heap: BinaryHeap::new(),
            blocked_by: None,
            pending_while_blocked: Vec::new(),
        }
    }

    /// This actor's identity. Hand it to every [`PortHandle`] built against
    /// this actor's channel so blocking callers can detect a re-entrant call
    /// from the actor's own thread.
    ///
    /// [`PortHandle`]: crate::port_handle::PortHandle
    pub fn id(&self) -> ActorId {
        self.id
    }

    /// Run the actor loop. Returns when the channel is closed (all senders dropped).
    /// Calls `shutdown()` on the driver before returning.
    #[cfg(test)]
    pub fn run(mut self) {
        let _scope = ActorScope::enter(self.id);
        loop {
            // Drain all pending messages into the heap
            self.drain_channel();

            if self.heap.is_empty() {
                // No work — block on the channel
                match self.rx.blocking_recv() {
                    Some(msg) => self.enqueue_message(msg),
                    None => break,
                }
                // Drain any more that arrived
                self.drain_channel();
            }

            // Process one eligible request from the heap
            self.process_one();
        }
        let _ = self.driver.shutdown();
    }

    /// Run the actor loop with a dedicated shutdown channel.
    /// Calls `shutdown()` on the driver before returning.
    ///
    /// A port stops for exactly two reasons, and dropping a
    /// [`PortRuntimeHandle`](crate::runtime::port::PortRuntimeHandle) is
    /// neither of them:
    ///
    /// - **Someone asked it to.** An explicit shutdown *sends* `()` on the
    ///   shutdown channel (`PortRuntimeHandle::shutdown`). It outranks every
    ///   outstanding handle: the port stops even while other `PortHandle`s
    ///   could still reach it.
    /// - **Nobody can reach it any more.** The request channel closed, i.e.
    ///   the last `PortHandle` was dropped. No request can ever arrive again,
    ///   so stopping is unobservable.
    ///
    /// The shutdown channel *closing* (every `PortRuntimeHandle` dropped) is
    /// deliberately NOT a stop condition. It used to be — closing was the only
    /// shutdown signal — which conflated "the creator dropped its handle" with
    /// "shut this port down" and killed ports that the registry still held a
    /// live `PortHandle` for.
    pub fn run_with_shutdown(mut self, mut shutdown_rx: mpsc::Receiver<()>) {
        // Publish our identity for the whole life of the actor thread: every
        // `PortDriver` method we dispatch below runs on this thread, so a
        // driver that calls back into its own port through a `PortHandle` can
        // be detected and refused instead of deadlocking on itself.
        let _scope = ActorScope::enter(self.id);
        // A *current-thread* runtime is deliberate: the actor owns its driver
        // exclusively and dispatches serially (C asyn's per-port `portThread`).
        // Nothing here may assume a multi-threaded runtime — see
        // `PortHandle::block_on_reply`.
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        rt.block_on(async {
            // Once every PortRuntimeHandle is gone no explicit shutdown can
            // ever arrive, so the (now permanently ready) shutdown branch is
            // disabled rather than spun on.
            let mut can_be_shut_down = true;
            loop {
                // Drain all pending messages into the heap
                self.drain_channel();

                if self.heap.is_empty() {
                    // Wait for a request, or for an explicit shutdown
                    tokio::select! {
                        msg = self.rx.recv() => {
                            match msg {
                                Some(m) => self.enqueue_message(m),
                                // Unreachable: the last PortHandle is gone.
                                None => break,
                            }
                        }
                        signal = shutdown_rx.recv(), if can_be_shut_down => {
                            match signal {
                                // Explicit shutdown.
                                Some(()) => break,
                                // Every PortRuntimeHandle was dropped. Not a
                                // shutdown — keep serving whoever still holds
                                // a PortHandle.
                                None => can_be_shut_down = false,
                            }
                        }
                    }
                    // Drain any more that arrived
                    self.drain_channel();
                }

                // Process one eligible request from the heap
                self.process_one();
            }
        });
        let _ = self.driver.shutdown();
    }

    fn drain_channel(&mut self) {
        while let Ok(msg) = self.rx.try_recv() {
            self.enqueue_message(msg);
        }
    }

    /// The asynManager methods that run **directly** under `asynManagerLock`
    /// rather than through `queueRequest` — connect/disconnect, enable/disable,
    /// auto-connect, the enable/auto-connect queries, block/unblock, and port
    /// shutdown (asynManager.c: `enable` 2222-2249, `autoConnectAsyn` 2310-2324,
    /// `isConnected`/`isEnabled` 2326-2354, `blockProcessCallback` 1692-1723).
    ///
    /// Because C never queues these, the block holder (`pblockProcessHolder`,
    /// which only gates the portThread's I/O `processUser` dispatch) cannot
    /// stall them, and the enabled/connected checks do not apply. This single
    /// predicate is the one owner of that classification: it governs the
    /// enabled/connected bypass in [`Self::process_one`] AND the block-divert
    /// exemption in [`Self::enqueue_message`] / the [`RequestOp::BlockProcess`]
    /// heap sweep, so both gates stay consistent.
    fn is_lifecycle_op(op: &RequestOp) -> bool {
        matches!(
            op,
            RequestOp::Connect
                | RequestOp::Disconnect
                | RequestOp::ConnectAddr
                | RequestOp::DisconnectAddr
                | RequestOp::EnableAddr
                | RequestOp::DisableAddr
                | RequestOp::SetEnable { .. }
                | RequestOp::SetAutoConnect { .. }
                | RequestOp::GetEnable
                | RequestOp::GetAutoConnect
                | RequestOp::BlockProcess
                | RequestOp::UnblockProcess
                | RequestOp::ShutdownPort
        )
    }

    fn enqueue_message(&mut self, msg: ActorMessage) {
        if let Some((owner, _)) = self.blocked_by {
            let is_owner = msg.block_token == Some(owner);
            // C parity: lifecycle/state ops are not queued, so the block
            // holder never stalls them — only non-owner I/O is diverted.
            // Previously only UnblockProcess was exempt, so a non-owner's
            // enable/auto-connect/connect/disconnect/get stalled until
            // UnblockProcess.
            if !is_owner && !Self::is_lifecycle_op(&msg.op) {
                self.pending_while_blocked.push(msg);
                return;
            }
        }
        self.heap.push(msg);
    }

    fn process_one(&mut self) {
        let msg = match self.heap.pop() {
            Some(m) => m,
            None => return,
        };

        let ActorMessage {
            op,
            mut user,
            cancel,
            reply,
            ..
        } = msg;

        // Claim the request for execution. This is the C dequeue under
        // `asynManagerLock`: `begin_running` transitions the cancel token
        // `Queued -> Running`, which closes the window in which an `AQR`
        // `cancelRequest` could report `wasQueued==1` (asynManager.c:1661-1666).
        // It fails only if the request was already cancelled while queued, in
        // which case it was removed from the queue and must be dropped.
        if !cancel.begin_running() {
            let _ = reply.send(Err(AsynError::Status {
                status: AsynStatus::Error,
                message: "request cancelled".into(),
            }));
            return;
        }
        // From here the request is running (C `callbackActive`): a concurrent
        // cancel can no longer win, and `finish` marks it `Done` on every exit
        // path so a late cancel stays a no-op and a multi-phase plan can
        // re-claim the token for its next phase.
        let _finish = FinishGuard(cancel);

        let is_connect_op = Self::is_lifecycle_op(&op);

        // C parity: a request that reaches the head of the queue always
        // executes — a dequeued request is never aborted by a queue
        // timeout. C arms a *queue-wait* timer only when queueRequest is
        // passed timeout > 0 (asynManager.c:1590-1623), and the port
        // thread cancels that timer the instant it dequeues the request
        // (:906/:827); standard device support passes
        // `queueRequest(..., 0.0)`, arming no timer at all. `user.timeout`
        // is the *I/O* timeout handed to the driver's read/write, not a
        // queue pre-execution deadline — reusing it as one aborted a
        // request that a slow predecessor had merely delayed past its I/O
        // budget. A genuine queue-wait timeout would be a separate async
        // timer, not derived from the I/O timeout.
        let is_connect_priority = user.priority == QueuePriority::Connect;

        // Connect ops and Connect-priority requests bypass enabled/connected checks
        // (C parity: Connect priority processed even when disabled/disconnected)
        if !is_connect_op && !is_connect_priority {
            // Auto-connect: try to reconnect if disconnected and
            // auto_connect is set, throttled to at most one attempt per 2s
            // window (C `autoConnectDevice`, asynManager.c:704-739).
            // Without the gate a burst of N queued requests to an offline
            // auto_connect port fires N back-to-back full connect attempts.
            // `auto_connect_throttle_ok` is the gate;
            // `stamp_auto_connect_attempt` restarts the window after the
            // attempt — success or failure — mirroring the C stamp at
            // :718/:735. The single-threaded actor owns the driver
            // throughout, so C's `autoConnectActive` re-entry guard has no
            // observable analogue here (no concurrent dispatch can re-enter).
            if self.driver.base().flags.multi_device {
                let ds = self.driver.base().device_states.get(&user.addr);
                let dev_disconnected = !ds.map_or(true, |d| d.connected);
                let dev_auto = ds.map_or(self.driver.base().auto_connect, |d| d.auto_connect);
                if dev_disconnected
                    && dev_auto
                    && self
                        .driver
                        .base()
                        .auto_connect_throttle_ok(user.addr, Instant::now())
                {
                    // For multi-device, auto-connect the specific address
                    let connect_user = AsynUser::new(user.reason).with_addr(user.addr);
                    let _ = self.driver.connect_addr(&connect_user);
                    self.driver
                        .base_mut()
                        .stamp_auto_connect_attempt(user.addr, Instant::now());
                }
            } else if !self.driver.base().connected
                && self.driver.base().auto_connect
                && self
                    .driver
                    .base()
                    .auto_connect_throttle_ok(-1, Instant::now())
            {
                let _ = self.driver.connect(&AsynUser::default());
                self.driver
                    .base_mut()
                    .stamp_auto_connect_attempt(-1, Instant::now());
            }

            // Check ready
            if let Err(e) = self.driver.base().check_ready_addr(user.addr) {
                let _ = reply.send(Err(e));
                return;
            }
        }

        // Dispatch
        let result = self.dispatch_io(&mut user, &op);
        let _ = reply.send(result);
    }

    fn dispatch_io(&mut self, user: &mut AsynUser, op: &RequestOp) -> AsynResult<RequestResult> {
        let is_read = matches!(
            op,
            RequestOp::Int32Read
                | RequestOp::Int64Read
                | RequestOp::Float64Read
                | RequestOp::OctetRead { .. }
                | RequestOp::OctetReadBinary { .. }
                | RequestOp::OctetWriteRead { .. }
                | RequestOp::UInt32DigitalRead { .. }
                | RequestOp::EnumRead
                | RequestOp::Int32ArrayRead { .. }
                | RequestOp::Float64ArrayRead { .. }
                | RequestOp::Int8ArrayRead { .. }
                | RequestOp::Int16ArrayRead { .. }
                | RequestOp::Int64ArrayRead { .. }
                | RequestOp::Float32ArrayRead { .. }
        );

        let result = match op {
            RequestOp::OctetWrite { data } => {
                let n = self.driver.io_write_octet(user, data)?;
                Ok(RequestResult::write_n(n))
            }
            RequestOp::OctetRead { buf_size } => {
                let mut buf = vec![0u8; *buf_size];
                let (n, eom) = self.driver.io_read_octet_eom(user, &mut buf)?;
                buf.truncate(n);
                Ok(RequestResult::octet_read_eom(buf, n, eom.bits()))
            }
            RequestOp::OctetWriteBinary { data } => {
                // C parity: asynRecord binary output (asynRecord.c:1528-1541).
                // Save the driver's output EOS, clear it for the raw write,
                // and restore it on every exit path so a configured OEOS does
                // not append terminator bytes to a binary payload. The actor
                // owns the bracket atomically under its serial dispatch.
                let saved = self.driver.get_output_eos();
                self.driver.set_output_eos(&[])?;
                let res = self.driver.io_write_octet(user, data);
                let _ = self.driver.set_output_eos(&saved);
                let n = res?;
                Ok(RequestResult::write_n(n))
            }
            RequestOp::OctetReadBinary { buf_size } => {
                // C parity: asynRecord binary input (asynRecord.c:1564-1577).
                // Save the driver's input EOS, clear it for the read, and
                // restore it on every exit path so a configured IEOS does not
                // stop the read early or strip bytes belonging to the binary
                // payload.
                let saved = self.driver.get_input_eos();
                self.driver.set_input_eos(&[])?;
                let mut buf = vec![0u8; *buf_size];
                let res = self.driver.io_read_octet_eom(user, &mut buf);
                let _ = self.driver.set_input_eos(&saved);
                let (n, eom) = res?;
                buf.truncate(n);
                Ok(RequestResult::octet_read_eom(buf, n, eom.bits()))
            }
            RequestOp::OctetWriteRead {
                data,
                buf_size,
                flush,
            } => {
                // Two C patterns share this op, distinguished by `flush`:
                //   flush=true  — asynOctetSyncIO::writeRead (asynOctetSyncIO.c:250)
                //     does flush() → write() → read() under a single
                //     queueLockPort. The flush drains stale bytes left in the
                //     driver's input buffer so the post-write read returns only
                //     the response to *this* command (the StreamDevice /
                //     asynRecord writeRead pattern).
                //   flush=false — devAsynOctet command-response
                //     (callbackSiCmdResponse, devAsynOctet.c:853-855) does plain
                //     writeIt → readIt on the raw asynOctet interface with NO
                //     flush, so the read returns whatever the device sends,
                //     including bytes already on the warm line.
                // Both run atomically here: the actor owns the port for the whole
                // op, the queueLockPort equivalent.
                if *flush {
                    self.driver.io_flush(user)?;
                }
                self.driver.io_write_octet(user, data)?;
                let mut buf = vec![0u8; *buf_size];
                let (n, eom) = self.driver.io_read_octet_eom(user, &mut buf)?;
                buf.truncate(n);
                Ok(RequestResult::octet_read_eom(buf, n, eom.bits()))
            }
            RequestOp::Int32Write { value } => {
                self.driver.io_write_int32(user, *value)?;
                Ok(RequestResult::write_ok())
            }
            RequestOp::Int32Read => {
                let v = self.driver.io_read_int32(user)?;
                Ok(RequestResult::int32_read(v))
            }
            RequestOp::Int64Write { value } => {
                self.driver.io_write_int64(user, *value)?;
                Ok(RequestResult::write_ok())
            }
            RequestOp::Int64Read => {
                let v = self.driver.io_read_int64(user)?;
                Ok(RequestResult::int64_read(v))
            }
            RequestOp::Float64Write { value } => {
                self.driver.io_write_float64(user, *value)?;
                Ok(RequestResult::write_ok())
            }
            RequestOp::Float64Read => {
                let v = self.driver.io_read_float64(user)?;
                Ok(RequestResult::float64_read(v))
            }
            RequestOp::UInt32DigitalWrite { value, mask } => {
                self.driver.io_write_uint32_digital(user, *value, *mask)?;
                Ok(RequestResult::write_ok())
            }
            RequestOp::UInt32DigitalRead { mask } => {
                let v = self.driver.io_read_uint32_digital(user, *mask)?;
                Ok(RequestResult::uint32_read(v))
            }
            RequestOp::Flush => {
                self.driver.io_flush(user)?;
                Ok(RequestResult::write_ok())
            }
            RequestOp::Connect => {
                self.driver.connect(user)?;
                Ok(RequestResult::write_ok())
            }
            RequestOp::Disconnect => {
                self.driver.disconnect(user)?;
                Ok(RequestResult::write_ok())
            }
            RequestOp::ShutdownPort => {
                // C `shutdownPort` lifecycle — opt-in via destructible
                // flag. Calls the driver's own shutdown() hook *after*
                // marking the lifecycle complete so the announcer sees
                // the port already-defunct.
                self.driver.base_mut().shutdown_lifecycle()?;
                // Driver's own shutdown plumbing (release hardware
                // handles, etc.). Errors are tolerated — the port is
                // already defunct and there is no recovery path.
                let _ = self.driver.shutdown();
                Ok(RequestResult::write_ok())
            }
            RequestOp::ConnectAddr => {
                self.driver.connect_addr(user)?;
                Ok(RequestResult::write_ok())
            }
            RequestOp::DisconnectAddr => {
                self.driver.disconnect_addr(user)?;
                Ok(RequestResult::write_ok())
            }
            RequestOp::EnableAddr => {
                self.driver.enable_addr(user)?;
                Ok(RequestResult::write_ok())
            }
            RequestOp::DisableAddr => {
                self.driver.disable_addr(user)?;
                Ok(RequestResult::write_ok())
            }
            RequestOp::SetEnable { yes } => {
                // C parity: pasynManager->enable(pasynUser, enable) at
                // asynManager.c — toggles per-port `enabled` state and
                // emits `asynExceptionEnable`. Routed through the
                // driver trait so subclasses can override.
                if *yes {
                    self.driver.enable(user)?;
                } else {
                    self.driver.disable(user)?;
                }
                Ok(RequestResult::write_ok())
            }
            RequestOp::SetAutoConnect { yes } => {
                // C parity: pasynManager->autoConnect(pasynUser, value)
                // at asynManager.c:2310-2324 — fires
                // `asynExceptionAutoConnect` unconditionally.
                self.driver.base_mut().set_auto_connect(*yes);
                Ok(RequestResult::write_ok())
            }
            RequestOp::GetEnable => {
                let enabled = self.driver.base().enabled;
                Ok(RequestResult::int32_read(i32::from(enabled)))
            }
            RequestOp::GetAutoConnect => {
                let auto = self.driver.base().auto_connect;
                Ok(RequestResult::int32_read(i32::from(auto)))
            }
            RequestOp::GetBoundsInt32 => {
                let (low, high) = self.driver.get_bounds_int32(user)?;
                Ok(RequestResult::bounds_read(low as i64, high as i64))
            }
            RequestOp::GetBoundsInt64 => {
                let (low, high) = self.driver.get_bounds_int64(user)?;
                Ok(RequestResult::bounds_read(low, high))
            }
            RequestOp::BlockProcess => {
                // Block-token contract: the block identity is
                // `user.block_token` when set, else it falls back to
                // `user.reason`. CALLER CONTRACT: any caller that
                // relies on block/unblock exclusivity MUST set a
                // distinct `block_token` — two callers that share a
                // `reason` and both omit `block_token` would collide
                // on the same fallback token, so one could
                // unblock/nest the other's lock. `block` /
                // `unblock` / and any owner-gated op must use the
                // SAME token; mismatched tokens are rejected below.
                let token = user.block_token.unwrap_or(user.reason as u64);
                if let Some((existing, ref mut count)) = self.blocked_by {
                    if existing == token {
                        // C parity: nested lock — increment counter
                        *count += 1;
                    } else {
                        return Err(AsynError::Status {
                            status: AsynStatus::Error,
                            message: "port already blocked by another user".into(),
                        });
                    }
                } else {
                    self.blocked_by = Some((token, 1));
                    // Messages already drained into the heap before this
                    // BlockProcess executed would otherwise still be
                    // dispatched to the driver, breaking block-port
                    // exclusivity. enqueue_message only diverts messages
                    // that arrive *after* the block, so sweep the heap now
                    // and divert every non-owner, non-unblock message.
                    let drained: Vec<ActorMessage> = self.heap.drain().collect();
                    for msg in drained {
                        let is_owner = msg.block_token == Some(token);
                        // Same exemption as enqueue_message: lifecycle/state
                        // ops are never gated by the block holder, so an
                        // already-heaped non-owner enable/connect/get is kept,
                        // not diverted to pending_while_blocked.
                        if is_owner || Self::is_lifecycle_op(&msg.op) {
                            self.heap.push(msg);
                        } else {
                            self.pending_while_blocked.push(msg);
                        }
                    }
                }
                Ok(RequestResult::write_ok())
            }
            RequestOp::UnblockProcess => {
                let token = user.block_token.unwrap_or(user.reason as u64);
                if let Some((owner, count)) = self.blocked_by {
                    if owner != token {
                        // C parity: only the block holder can unblock
                        return Err(AsynError::Status {
                            status: AsynStatus::Error,
                            message: "unblock rejected: not the block holder".into(),
                        });
                    }
                    if count > 1 {
                        self.blocked_by = Some((owner, count - 1));
                    } else {
                        self.blocked_by = None;
                        let pending = std::mem::take(&mut self.pending_while_blocked);
                        for msg in pending {
                            self.heap.push(msg);
                        }
                    }
                }
                Ok(RequestResult::write_ok())
            }
            RequestOp::DrvUserCreate(req) => {
                let info = self.driver.drv_user_create(req)?;
                Ok(RequestResult::drv_user_create(
                    info.reason,
                    info.max_octet_len,
                ))
            }
            RequestOp::EnumRead => {
                // Carry the driver's enum table (strings/values/severities)
                // alongside the current index so device-support init can push
                // it onto the record's state fields — C devAsynInt32.c::initCommon
                // reads asynEnum and calls setEnums (297-324, 415-435). Dropping
                // the table left mbbi/mbbo/bi/bo with their .db state strings.
                let (idx, entries) = self.driver.read_enum(user)?;
                Ok(RequestResult::enum_read_with_entries(idx, entries))
            }
            RequestOp::EnumWrite { index } => {
                self.driver.write_enum(user, *index)?;
                Ok(RequestResult::write_ok())
            }
            RequestOp::Int32ArrayRead { max_elements } => {
                let mut buf = vec![0i32; *max_elements];
                let n = self.driver.read_int32_array(user, &mut buf)?;
                buf.truncate(n);
                Ok(RequestResult::int32_array_read(buf))
            }
            RequestOp::Int32ArrayWrite { data } => {
                self.driver.write_int32_array(user, data)?;
                Ok(RequestResult::write_ok())
            }
            RequestOp::Float64ArrayRead { max_elements } => {
                let mut buf = vec![0f64; *max_elements];
                let n = self.driver.read_float64_array(user, &mut buf)?;
                buf.truncate(n);
                Ok(RequestResult::float64_array_read(buf))
            }
            RequestOp::Float64ArrayWrite { data } => {
                self.driver.write_float64_array(user, data)?;
                Ok(RequestResult::write_ok())
            }
            RequestOp::Int8ArrayRead { max_elements } => {
                let mut buf = vec![0i8; *max_elements];
                let n = self.driver.read_int8_array(user, &mut buf)?;
                buf.truncate(n);
                Ok(RequestResult::int8_array_read(buf))
            }
            RequestOp::Int8ArrayWrite { data } => {
                self.driver.write_int8_array(user, data)?;
                Ok(RequestResult::write_ok())
            }
            RequestOp::Int16ArrayRead { max_elements } => {
                let mut buf = vec![0i16; *max_elements];
                let n = self.driver.read_int16_array(user, &mut buf)?;
                buf.truncate(n);
                Ok(RequestResult::int16_array_read(buf))
            }
            RequestOp::Int16ArrayWrite { data } => {
                self.driver.write_int16_array(user, data)?;
                Ok(RequestResult::write_ok())
            }
            RequestOp::Int64ArrayRead { max_elements } => {
                let mut buf = vec![0i64; *max_elements];
                let n = self.driver.read_int64_array(user, &mut buf)?;
                buf.truncate(n);
                Ok(RequestResult::int64_array_read(buf))
            }
            RequestOp::Int64ArrayWrite { data } => {
                self.driver.write_int64_array(user, data)?;
                Ok(RequestResult::write_ok())
            }
            RequestOp::Float32ArrayRead { max_elements } => {
                let mut buf = vec![0f32; *max_elements];
                let n = self.driver.read_float32_array(user, &mut buf)?;
                buf.truncate(n);
                Ok(RequestResult::float32_array_read(buf))
            }
            RequestOp::Float32ArrayWrite { data } => {
                self.driver.write_float32_array(user, data)?;
                Ok(RequestResult::write_ok())
            }
            RequestOp::CallParamCallbacks { addr, updates } => {
                let base = self.driver.base_mut();
                // One dispatch for every parameter type (`ParamList::set_value`),
                // so this carrier and the store cannot support different type sets.
                let mut failed: Option<AsynError> = None;
                for u in updates {
                    let outcome = match u {
                        crate::request::ParamSetValue::Value {
                            reason,
                            addr,
                            value,
                        } => base.params.set_value(*reason, *addr, value.clone()),
                        crate::request::ParamSetValue::UInt32Digital {
                            reason,
                            addr,
                            value,
                            mask,
                            interrupt_mask,
                        } => base.set_uint32_param(*reason, *addr, *value, *mask, *interrupt_mask),
                    };
                    // A set that fails (a value the parameter cannot hold) is
                    // reported, not swallowed: C `asynPortDriver::setIntegerParam`
                    // asynPrints the paramList error at ASYN_TRACE_ERROR and
                    // returns the status. The remaining updates still land and the
                    // callbacks still fire — one bad update must not strand the
                    // good ones — and the first error is returned to whoever waits
                    // on the reply.
                    if let Err(e) = outcome {
                        if let Some(trace) = base.trace.clone() {
                            trace.output(
                                &base.port_name,
                                crate::trace::TraceMask::ERROR,
                                &format!("callParamCallbacks: param set failed: {e}\n"),
                            );
                        }
                        failed.get_or_insert(e);
                    }
                }
                base.call_param_callbacks(*addr)?;
                match failed {
                    Some(e) => Err(e),
                    None => Ok(RequestResult::write_ok()),
                }
            }
            RequestOp::GetOption { key } => {
                let val = self.driver.get_option(key)?;
                Ok(RequestResult::option_read(val))
            }
            RequestOp::SetOption { key, value } => {
                self.driver.set_option(key, value)?;
                Ok(RequestResult::write_ok())
            }
            RequestOp::Report { level } => {
                // C parity: `asynManager::report` (asynManager.c) walks
                // every registered port and calls each driver's
                // `pasynCommon->report` callback. The iocsh wrapper
                // (`asynReport`) does the per-port loop; here we
                // dispatch the per-port `report(level)` from the
                // actor thread so the driver observes its own state
                // under the actor's serial ownership.
                self.driver.report(*level);
                Ok(RequestResult::write_ok())
            }
            RequestOp::SetInputEos { eos } => {
                // C parity: asynRecord IEOS write at asynRecord.c:391
                // calls `pasynOctet->setInputEos(pasynUser, eos, len)`.
                // Route through the driver trait so the EOS interpose
                // layer (interpose/eos.rs) reads from a single source
                // of truth (`PortDriverBase::input_eos`) rather than
                // the orphaned options HashMap.
                self.driver.set_input_eos(eos)?;
                Ok(RequestResult::write_ok())
            }
            RequestOp::SetOutputEos { eos } => {
                self.driver.set_output_eos(eos)?;
                Ok(RequestResult::write_ok())
            }
        };

        // Attach alarm/timestamp metadata on successful reads
        if is_read {
            if let Ok(r) = result {
                let (status, alarm_status, alarm_severity) = self
                    .driver
                    .base()
                    .params
                    .get_param_status(user.reason, user.addr)
                    .unwrap_or((crate::error::AsynStatus::Success, 0, 0));
                let ts = self
                    .driver
                    .base()
                    .params
                    .get_timestamp(user.reason, user.addr)
                    .unwrap_or(None);
                // C devAsynInt32.c:844-847 — a non-success read/param status
                // maps to a record alarm (asynStatusToEpicsAlarm) and is
                // recGblSetSevr'd together with the explicit setParamAlarm.
                // The old `_` discarded the status, so a setParamStatus(error
                // /timeout) read returned clean (no INVALID, UDF-clearing).
                let (alarm_status, alarm_severity) =
                    combine_read_alarm(status, alarm_status, alarm_severity);
                // Carry the device read status (C pasynUser->auxStatus) to the
                // device-support consumer so it can gate the value store the way
                // C processAi does (store iff result.status == asynSuccess,
                // devAsynInt32.c:848-855). Kept distinct from `status` (the
                // op/request outcome) so the network reply path — which keys on
                // `status` to emit an Error reply — is unaffected.
                let mut out = r.with_alarm(alarm_status, alarm_severity, ts);
                out.aux_status = status;
                return Ok(out);
            }
        }

        result
    }
}

/// Combine a param's stored asynStatus with its explicit setParamAlarm into
/// the record alarm for a READ, mirroring C `asynStatusToEpicsAlarm`
/// (asynEpicsUtils.c:234) as invoked by devAsynXxx processXxx
/// (devAsynInt32.c:844-847): the explicit alarm wins per field, and a
/// non-success status fills any field still None — status maps to a
/// condition with READ_ALARM as the asynError/default condition and INVALID
/// as the severity.
fn combine_read_alarm(status: AsynStatus, alarm_status: u16, alarm_severity: u16) -> (u16, u16) {
    use epics_base_rs::server::recgbl::alarm_status as al;
    use epics_base_rs::server::record::AlarmSeverity;
    let stat_default = match status {
        AsynStatus::Success => return (alarm_status, alarm_severity),
        AsynStatus::Timeout => al::TIMEOUT_ALARM,
        AsynStatus::Overflow => al::HW_LIMIT_ALARM,
        AsynStatus::Disconnected => al::COMM_ALARM,
        AsynStatus::Disabled => al::DISABLE_ALARM,
        AsynStatus::Error => al::READ_ALARM,
    };
    let stat = if alarm_status == al::NO_ALARM {
        stat_default
    } else {
        alarm_status
    };
    let sevr = if alarm_severity == AlarmSeverity::NoAlarm as u16 {
        AlarmSeverity::Invalid as u16
    } else {
        alarm_severity
    };
    (stat, sevr)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::param::ParamType;
    use crate::port::{PortDriverBase, PortFlags};
    use std::sync::Arc;
    use std::time::Duration;

    struct TestDriver {
        base: PortDriverBase,
    }

    impl TestDriver {
        fn new() -> Self {
            let mut base = PortDriverBase::new("actor_test", 1, PortFlags::default());
            base.create_param("VAL", ParamType::Int32).unwrap();
            base.create_param("F64", ParamType::Float64).unwrap();
            base.create_param("MSG", ParamType::Octet).unwrap();
            base.create_param("BIG", ParamType::Int64).unwrap();
            Self { base }
        }
    }

    impl PortDriver for TestDriver {
        fn base(&self) -> &PortDriverBase {
            &self.base
        }
        fn base_mut(&mut self) -> &mut PortDriverBase {
            &mut self.base
        }
    }

    fn spawn_actor(driver: impl PortDriver) -> mpsc::Sender<ActorMessage> {
        let (tx, rx) = mpsc::channel(256);
        let actor = PortActor::new(Box::new(driver), rx);
        std::thread::Builder::new()
            .name("test-actor".into())
            .spawn(move || actor.run())
            .unwrap();
        tx
    }

    fn send_and_wait(
        tx: &mpsc::Sender<ActorMessage>,
        op: RequestOp,
        user: AsynUser,
    ) -> AsynResult<RequestResult> {
        let (reply_tx, reply_rx) = oneshot::channel();
        let msg = ActorMessage::new(op, user, CancelToken::new(), reply_tx);
        tx.blocking_send(msg).expect("actor channel closed");
        reply_rx.blocking_recv().expect("actor dropped reply")
    }

    #[test]
    fn actor_int32_write_read() {
        let tx = spawn_actor(TestDriver::new());
        let user = AsynUser::new(0).with_timeout(Duration::from_secs(1));
        send_and_wait(&tx, RequestOp::Int32Write { value: 42 }, user).unwrap();

        let user = AsynUser::new(0).with_timeout(Duration::from_secs(1));
        let result = send_and_wait(&tx, RequestOp::Int32Read, user).unwrap();
        assert_eq!(result.int_val, Some(42));
    }

    #[test]
    fn combine_read_alarm_matches_c_status_to_epics_alarm() {
        use epics_base_rs::server::recgbl::alarm_status as al;
        use epics_base_rs::server::record::AlarmSeverity;
        let none = AlarmSeverity::NoAlarm as u16;
        let major = AlarmSeverity::Major as u16;
        let invalid = AlarmSeverity::Invalid as u16;
        // Success: the explicit alarm passes through unchanged (incl. clean).
        assert_eq!(
            combine_read_alarm(AsynStatus::Success, al::NO_ALARM, none),
            (al::NO_ALARM, none)
        );
        assert_eq!(
            combine_read_alarm(AsynStatus::Success, al::HW_LIMIT_ALARM, major),
            (al::HW_LIMIT_ALARM, major)
        );
        // Non-success, no explicit alarm: status-derived condition + INVALID.
        assert_eq!(
            combine_read_alarm(AsynStatus::Error, al::NO_ALARM, none),
            (al::READ_ALARM, invalid)
        );
        assert_eq!(
            combine_read_alarm(AsynStatus::Timeout, al::NO_ALARM, none),
            (al::TIMEOUT_ALARM, invalid)
        );
        assert_eq!(
            combine_read_alarm(AsynStatus::Overflow, al::NO_ALARM, none),
            (al::HW_LIMIT_ALARM, invalid)
        );
        assert_eq!(
            combine_read_alarm(AsynStatus::Disconnected, al::NO_ALARM, none),
            (al::COMM_ALARM, invalid)
        );
        assert_eq!(
            combine_read_alarm(AsynStatus::Disabled, al::NO_ALARM, none),
            (al::DISABLE_ALARM, invalid)
        );
        // Explicit alarm present wins per field over the status-derived one.
        assert_eq!(
            combine_read_alarm(AsynStatus::Error, al::COMM_ALARM, major),
            (al::COMM_ALARM, major)
        );
    }

    #[test]
    fn actor_read_surfaces_param_status_alarm() {
        use epics_base_rs::server::recgbl::alarm_status as al;
        use epics_base_rs::server::record::AlarmSeverity;
        // A defined param flagged setParamStatus(Error) with no explicit
        // setParamAlarm: the read must surface READ_ALARM/INVALID (C
        // devAsynInt32.c:844-847), not a clean (UDF-clearing) read.
        let mut drv = TestDriver::new();
        drv.base.params.set_int32(0, 0, 5).unwrap(); // define VAL
        drv.base
            .params
            .set_param_status(0, 0, AsynStatus::Error, 0, 0)
            .unwrap();
        let tx = spawn_actor(drv);

        let user = AsynUser::new(0).with_timeout(Duration::from_secs(1));
        let result = send_and_wait(&tx, RequestOp::Int32Read, user).unwrap();
        assert_eq!(result.int_val, Some(5));
        assert_eq!(result.alarm_status, al::READ_ALARM);
        assert_eq!(result.alarm_severity, AlarmSeverity::Invalid as u16);
        // The transport status is also carried as aux_status (distinct from the
        // op-level `status`, which stays Success) so device support can gate the
        // value store on it — C processAi (devAsynInt32.c:848-855).
        assert_eq!(result.aux_status, AsynStatus::Error);
        assert_eq!(
            result.status,
            AsynStatus::Success,
            "op-level status stays Success so the network reply path is unaffected"
        );
    }

    #[test]
    fn actor_float64_write_read() {
        let tx = spawn_actor(TestDriver::new());
        let user = AsynUser::new(1).with_timeout(Duration::from_secs(1));
        send_and_wait(&tx, RequestOp::Float64Write { value: 3.14 }, user).unwrap();

        let user = AsynUser::new(1).with_timeout(Duration::from_secs(1));
        let result = send_and_wait(&tx, RequestOp::Float64Read, user).unwrap();
        assert!((result.float_val.unwrap() - 3.14).abs() < 1e-10);
    }

    #[test]
    fn actor_int64_write_read() {
        let tx = spawn_actor(TestDriver::new());
        let user = AsynUser::new(3).with_timeout(Duration::from_secs(1));
        send_and_wait(&tx, RequestOp::Int64Write { value: i64::MAX }, user).unwrap();

        let user = AsynUser::new(3).with_timeout(Duration::from_secs(1));
        let result = send_and_wait(&tx, RequestOp::Int64Read, user).unwrap();
        assert_eq!(result.int64_val, Some(i64::MAX));
    }

    #[test]
    fn actor_octet_write_read() {
        let tx = spawn_actor(TestDriver::new());
        let user = AsynUser::new(2).with_timeout(Duration::from_secs(1));
        send_and_wait(
            &tx,
            RequestOp::OctetWrite {
                data: b"hello".to_vec(),
            },
            user,
        )
        .unwrap();

        let user = AsynUser::new(2).with_timeout(Duration::from_secs(1));
        let result = send_and_wait(&tx, RequestOp::OctetRead { buf_size: 256 }, user).unwrap();
        assert_eq!(&result.data.unwrap()[..5], b"hello");
    }

    /// C parity: asynRecord binary I/O (asynRecord.c:1528-1577) saves the
    /// driver EOS, clears it for the raw transfer, and restores it. The
    /// OctetWriteBinary/OctetReadBinary ops must observe an empty EOS during
    /// the binary transfer and leave the configured EOS intact afterward.
    #[test]
    fn actor_octet_binary_io_suppresses_and_restores_eos() {
        struct EosObserver {
            base: PortDriverBase,
            write_eos: Arc<parking_lot::Mutex<Vec<u8>>>,
            read_eos: Arc<parking_lot::Mutex<Vec<u8>>>,
        }
        impl PortDriver for EosObserver {
            fn base(&self) -> &PortDriverBase {
                &self.base
            }
            fn base_mut(&mut self) -> &mut PortDriverBase {
                &mut self.base
            }
            fn io_write_octet(&mut self, _user: &mut AsynUser, data: &[u8]) -> AsynResult<usize> {
                *self.write_eos.lock() = self.base().output_eos.clone();
                Ok(data.len())
            }
            fn io_read_octet(&mut self, _user: &AsynUser, buf: &mut [u8]) -> AsynResult<usize> {
                *self.read_eos.lock() = self.base().input_eos.clone();
                let resp = [0x01u8, 0x02];
                let n = resp.len().min(buf.len());
                buf[..n].copy_from_slice(&resp[..n]);
                Ok(n)
            }
        }

        let write_eos = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let read_eos = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let tx = spawn_actor(EosObserver {
            base: PortDriverBase::new("eos_test", 1, PortFlags::default()),
            write_eos: write_eos.clone(),
            read_eos: read_eos.clone(),
        });
        let mk = || AsynUser::new(0).with_timeout(Duration::from_secs(1));

        send_and_wait(
            &tx,
            RequestOp::SetOutputEos {
                eos: b"\r\n".to_vec(),
            },
            mk(),
        )
        .unwrap();
        send_and_wait(
            &tx,
            RequestOp::SetInputEos {
                eos: b"\n".to_vec(),
            },
            mk(),
        )
        .unwrap();

        // Binary transfers observe a suppressed EOS.
        send_and_wait(
            &tx,
            RequestOp::OctetWriteBinary {
                data: vec![0x00, 0x01],
            },
            mk(),
        )
        .unwrap();
        assert!(
            write_eos.lock().is_empty(),
            "binary write must see the output EOS suppressed"
        );
        send_and_wait(&tx, RequestOp::OctetReadBinary { buf_size: 16 }, mk()).unwrap();
        assert!(
            read_eos.lock().is_empty(),
            "binary read must see the input EOS suppressed"
        );

        // A subsequent non-binary transfer sees the restored EOS.
        send_and_wait(
            &tx,
            RequestOp::OctetWrite {
                data: b"x".to_vec(),
            },
            mk(),
        )
        .unwrap();
        assert_eq!(
            &*write_eos.lock(),
            b"\r\n",
            "output EOS must be restored after a binary write"
        );
        send_and_wait(&tx, RequestOp::OctetRead { buf_size: 16 }, mk()).unwrap();
        assert_eq!(
            &*read_eos.lock(),
            b"\n",
            "input EOS must be restored after a binary read"
        );
    }

    /// C parity: asynOctetSyncIO::writeRead (asynOctetSyncIO.c:250)
    /// calls flush() before write() so any stale input bytes (echoes,
    /// half-received responses from a previous command) are drained
    /// out of the driver's input buffer before the new write+read
    /// pair. The atomic OctetWriteRead op must do the same.
    #[test]
    fn actor_octet_write_read_calls_flush_first() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct FlushTracker {
            base: PortDriverBase,
            flush_calls: Arc<AtomicUsize>,
            write_calls: Arc<AtomicUsize>,
            sequence: Arc<parking_lot::Mutex<Vec<&'static str>>>,
        }
        impl PortDriver for FlushTracker {
            fn base(&self) -> &PortDriverBase {
                &self.base
            }
            fn base_mut(&mut self) -> &mut PortDriverBase {
                &mut self.base
            }
            fn io_flush(&mut self, _user: &mut AsynUser) -> AsynResult<()> {
                self.flush_calls.fetch_add(1, Ordering::Relaxed);
                self.sequence.lock().push("flush");
                Ok(())
            }
            fn io_write_octet(&mut self, _user: &mut AsynUser, data: &[u8]) -> AsynResult<usize> {
                self.write_calls.fetch_add(1, Ordering::Relaxed);
                self.sequence.lock().push("write");
                Ok(data.len())
            }
            fn io_read_octet(&mut self, _user: &AsynUser, buf: &mut [u8]) -> AsynResult<usize> {
                self.sequence.lock().push("read");
                let resp = b"RSP";
                let n = resp.len().min(buf.len());
                buf[..n].copy_from_slice(&resp[..n]);
                Ok(n)
            }
        }

        let flush_calls = Arc::new(AtomicUsize::new(0));
        let write_calls = Arc::new(AtomicUsize::new(0));
        let sequence = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let drv = FlushTracker {
            base: PortDriverBase::new("flush_test", 1, PortFlags::default()),
            flush_calls: flush_calls.clone(),
            write_calls: write_calls.clone(),
            sequence: sequence.clone(),
        };
        let tx = spawn_actor(drv);
        let user = AsynUser::new(0).with_timeout(Duration::from_secs(1));
        let result = send_and_wait(
            &tx,
            RequestOp::OctetWriteRead {
                data: b"CMD".to_vec(),
                buf_size: 16,
                flush: true,
            },
            user,
        )
        .unwrap();
        assert_eq!(result.data.unwrap(), b"RSP".to_vec());
        assert_eq!(flush_calls.load(Ordering::Relaxed), 1, "flush called once");
        assert_eq!(write_calls.load(Ordering::Relaxed), 1);
        // Order must be: flush, then write, then read.
        let seq = sequence.lock().clone();
        assert_eq!(seq, vec!["flush", "write", "read"]);
    }

    /// C parity: `asynOctet::read` returns `nbytes` together with
    /// `int *eomReason` (`interfaces/asynOctet.h:38-40`). Drivers
    /// that override `io_read_octet_eom` must have their EOM flags
    /// propagated through `RequestResult::eom_reason` so consumers
    /// (e.g. asynRecord `EOMR`) can distinguish "byte count" /
    /// "EOS match" / "END indicator" terminations.
    #[test]
    fn actor_octet_read_eom_propagates_driver_flags() {
        use crate::interpose::EomReason;

        struct EomDriver {
            base: PortDriverBase,
        }
        impl PortDriver for EomDriver {
            fn base(&self) -> &PortDriverBase {
                &self.base
            }
            fn base_mut(&mut self) -> &mut PortDriverBase {
                &mut self.base
            }
            fn io_read_octet_eom(
                &mut self,
                _user: &AsynUser,
                buf: &mut [u8],
            ) -> AsynResult<(usize, EomReason)> {
                let payload = b"ABC";
                let n = payload.len().min(buf.len());
                buf[..n].copy_from_slice(&payload[..n]);
                // Driver explicitly reports both EOS match and END indicator
                // (e.g. a GPIB END line plus terminator detection).
                Ok((n, EomReason::EOS | EomReason::END))
            }
        }
        let drv = EomDriver {
            base: PortDriverBase::new("eom_test", 1, PortFlags::default()),
        };
        let tx = spawn_actor(drv);
        let user = AsynUser::new(0).with_timeout(Duration::from_secs(1));
        let result = send_and_wait(&tx, RequestOp::OctetRead { buf_size: 16 }, user).unwrap();
        assert_eq!(result.data.as_deref(), Some(&b"ABC"[..]));
        let eom = EomReason::from_bits_truncate(result.eom_reason);
        assert!(eom.contains(EomReason::EOS));
        assert!(eom.contains(EomReason::END));
        assert!(!eom.contains(EomReason::CNT));
    }

    /// Default `io_read_octet_eom` (drivers that override only the
    /// non-eom `io_read_octet`) must synthesize `CNT` when the
    /// buffer filled — C `asynOctetSyncIO::read` synthesises the
    /// same flag at the syncIO level (`asynOctetSyncIO.c:213-217`).
    #[test]
    fn actor_octet_read_eom_default_synthesizes_cnt_on_buffer_full() {
        use crate::interpose::EomReason;

        struct FillDriver {
            base: PortDriverBase,
        }
        impl PortDriver for FillDriver {
            fn base(&self) -> &PortDriverBase {
                &self.base
            }
            fn base_mut(&mut self) -> &mut PortDriverBase {
                &mut self.base
            }
            fn io_read_octet(&mut self, _user: &AsynUser, buf: &mut [u8]) -> AsynResult<usize> {
                // Always fill the buffer to completion.
                for b in buf.iter_mut() {
                    *b = b'X';
                }
                Ok(buf.len())
            }
        }
        let drv = FillDriver {
            base: PortDriverBase::new("fill_test", 1, PortFlags::default()),
        };
        let tx = spawn_actor(drv);
        let user = AsynUser::new(0).with_timeout(Duration::from_secs(1));
        let result = send_and_wait(&tx, RequestOp::OctetRead { buf_size: 4 }, user).unwrap();
        assert_eq!(result.nbytes, 4);
        let eom = EomReason::from_bits_truncate(result.eom_reason);
        assert!(eom.contains(EomReason::CNT));
        assert!(!eom.contains(EomReason::EOS));
        assert!(!eom.contains(EomReason::END));
    }

    #[test]
    fn heap_pops_fifo_within_priority() {
        // C asynManager services each priority as a strict FIFO (queueRequest
        // ellAdd to the tail; portThread walks ellFirst->ellNext): submission
        // order is preserved within a priority by the seq tiebreaker alone.
        let mk = |seq: u64| {
            let (reply, _rx) = oneshot::channel();
            ActorMessage {
                op: RequestOp::Int32Read,
                user: AsynUser::new(0),
                cancel: CancelToken::new(),
                reply,
                seq,
                priority: QueuePriority::Medium,
                block_token: None,
            }
        };
        let mut heap = BinaryHeap::new();
        // seq 0 submitted before seq 1, same priority.
        heap.push(mk(0));
        heap.push(mk(1));
        // FIFO: seq 0 pops first.
        assert_eq!(heap.pop().unwrap().seq, 0);
        assert_eq!(heap.pop().unwrap().seq, 1);
    }

    #[test]
    fn heap_pops_higher_priority_first() {
        // Priority dominates the seq FIFO tiebreaker.
        let mk = |seq: u64, priority: QueuePriority| {
            let (reply, _rx) = oneshot::channel();
            ActorMessage {
                op: RequestOp::Int32Read,
                user: AsynUser::new(0),
                cancel: CancelToken::new(),
                reply,
                seq,
                priority,
                block_token: None,
            }
        };
        let mut heap = BinaryHeap::new();
        heap.push(mk(0, QueuePriority::Low));
        heap.push(mk(1, QueuePriority::High));
        heap.push(mk(2, QueuePriority::Medium));
        // High then Medium then Low, regardless of submission order.
        assert_eq!(heap.pop().unwrap().seq, 1);
        assert_eq!(heap.pop().unwrap().seq, 2);
        assert_eq!(heap.pop().unwrap().seq, 0);
    }

    #[test]
    fn actor_cancel() {
        let tx = spawn_actor(TestDriver::new());
        let cancel = CancelToken::new();
        cancel.cancel();
        let user = AsynUser::new(0).with_timeout(Duration::from_secs(1));
        let (reply_tx, reply_rx) = oneshot::channel();
        let msg = ActorMessage::new(RequestOp::Int32Read, user, cancel, reply_tx);
        tx.blocking_send(msg).unwrap();
        let result = reply_rx.blocking_recv().unwrap();
        assert!(result.is_err());
    }

    #[test]
    fn actor_dequeued_request_runs_despite_elapsed_timeout() {
        // C parity: a request that reaches the head of the queue always
        // executes I/O — the queue-wait timer (when armed at all) is
        // cancelled at dequeue (asynManager.c:906), and standard device
        // support passes queueRequest(..., 0.0) so no timer exists. The
        // I/O `user.timeout` must NOT abort a dequeued request just because
        // a slow predecessor delayed it past that budget.
        let mut drv = TestDriver::new();
        drv.base.params.set_int32(0, 0, 7).unwrap();
        let tx = spawn_actor(drv);
        // Tiny I/O timeout; let it lapse before the request is serviced.
        let user = AsynUser::new(0).with_timeout(Duration::from_nanos(1));
        std::thread::sleep(Duration::from_millis(1));
        let result = send_and_wait(&tx, RequestOp::Int32Read, user);
        // No queue-deadline abort: the read runs and returns the value.
        assert_eq!(result.unwrap().int_val, Some(7));
    }

    #[test]
    fn actor_disabled_port() {
        let mut drv = TestDriver::new();
        drv.base.enabled = false;
        let tx = spawn_actor(drv);
        let user = AsynUser::new(0).with_timeout(Duration::from_secs(1));
        let result = send_and_wait(&tx, RequestOp::Int32Read, user);
        match result {
            Err(AsynError::Status { status, .. }) => assert_eq!(status, AsynStatus::Disabled),
            other => panic!("expected Disabled, got {other:?}"),
        }
    }

    #[test]
    fn actor_setenable_refuses_defunct_port() {
        // SetEnable is a lifecycle op that bypasses check_ready's
        // defunct-first gate, so the refusal must come from the enable
        // owner itself (C `enable`, asynManager.c:2236-2241). A defunct
        // port must answer SetEnable with asynDisabled, not silently
        // re-enable.
        let mut drv = TestDriver::new();
        drv.base.defunct = true;
        let tx = spawn_actor(drv);
        let user = AsynUser::new(0).with_timeout(Duration::from_secs(1));
        let result = send_and_wait(&tx, RequestOp::SetEnable { yes: true }, user);
        match result {
            Err(AsynError::Status { status, .. }) => assert_eq!(status, AsynStatus::Disabled),
            other => panic!("expected Disabled, got {other:?}"),
        }
    }

    #[test]
    fn actor_auto_connect() {
        let mut drv = TestDriver::new();
        drv.base.connected = false;
        drv.base.auto_connect = true;
        // Seed VAL so the read probe returns a value: the default read now
        // surfaces an unset param as ParamUndefined (C parity), so this
        // lifecycle test must assert the auto-connect outcome (the request
        // reaches the driver and is not rejected), not undefined-read state.
        drv.base.params.set_int32(0, 0, 0).unwrap();
        let tx = spawn_actor(drv);
        let user = AsynUser::new(0).with_timeout(Duration::from_secs(1));
        let result = send_and_wait(&tx, RequestOp::Int32Read, user);
        assert!(result.is_ok());
    }

    #[test]
    fn actor_auto_connect_throttled_to_one_attempt_per_window() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        // A driver whose `connect()` counts the attempt but leaves the port
        // disconnected — simulating a hardware reconnect that keeps failing.
        // Without the 2s throttle (C autoConnectDevice, asynManager.c:713),
        // every queued request to an offline auto_connect port would fire a
        // fresh full connect attempt.
        struct ThrottleDriver {
            base: PortDriverBase,
            connect_calls: Arc<AtomicUsize>,
        }
        impl PortDriver for ThrottleDriver {
            fn base(&self) -> &PortDriverBase {
                &self.base
            }
            fn base_mut(&mut self) -> &mut PortDriverBase {
                &mut self.base
            }
            fn connect(&mut self, _user: &AsynUser) -> AsynResult<()> {
                self.connect_calls.fetch_add(1, Ordering::SeqCst);
                // Stay disconnected: the throttle, not a state change, must
                // be what bounds retries.
                Ok(())
            }
        }

        let connect_calls = Arc::new(AtomicUsize::new(0));
        let mut base = PortDriverBase::new("throttle_test", 1, PortFlags::default());
        base.create_param("VAL", ParamType::Int32).unwrap();
        base.connected = false;
        base.auto_connect = true;
        let drv = ThrottleDriver {
            base,
            connect_calls: connect_calls.clone(),
        };
        let tx = spawn_actor(drv);

        // Three back-to-back reads to the disconnected auto_connect port.
        for _ in 0..3 {
            let user = AsynUser::new(0).with_timeout(Duration::from_secs(1));
            let result = send_and_wait(&tx, RequestOp::Int32Read, user);
            // The port never reconnects, so each request fails Disconnected
            // (C autoConnectDevice returns FALSE => queueRequest fails).
            match result {
                Err(AsynError::Status { status, .. }) => {
                    assert_eq!(status, AsynStatus::Disconnected)
                }
                other => panic!("expected Disconnected, got {other:?}"),
            }
        }

        // Only the first request's attempt fired; the second and third fell
        // inside the 2s throttle window and were refused without a connect
        // call (C autoConnectDevice 2.0s gate, asynManager.c:712-713).
        assert_eq!(connect_calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn actor_get_enable_and_auto_connect() {
        // GetEnable/GetAutoConnect report the driver's actual state and
        // answer even when the port is disabled (they are lifecycle ops
        // that bypass the enabled/connected check).
        let mut drv = TestDriver::new();
        drv.base.enabled = false;
        drv.base.auto_connect = true;
        let tx = spawn_actor(drv);

        let user = AsynUser::new(0).with_timeout(Duration::from_secs(1));
        let r = send_and_wait(&tx, RequestOp::GetEnable, user).unwrap();
        assert_eq!(r.int_val, Some(0));

        let user = AsynUser::new(0).with_timeout(Duration::from_secs(1));
        let r = send_and_wait(&tx, RequestOp::GetAutoConnect, user).unwrap();
        assert_eq!(r.int_val, Some(1));
    }

    #[test]
    fn actor_connect_disconnect() {
        let mut drv = TestDriver::new();
        // Seed VAL so the post-reconnect read probe returns a value rather
        // than ParamUndefined; this test asserts the reconnect lifecycle.
        drv.base.params.set_int32(0, 0, 0).unwrap();
        let tx = spawn_actor(drv);

        let user = AsynUser::new(0).with_timeout(Duration::from_secs(1));
        send_and_wait(&tx, RequestOp::Disconnect, user).unwrap();

        // Port is now disconnected, auto_connect is true by default
        let user = AsynUser::new(0).with_timeout(Duration::from_secs(1));
        send_and_wait(&tx, RequestOp::Connect, user).unwrap();

        let user = AsynUser::new(0).with_timeout(Duration::from_secs(1));
        let result = send_and_wait(&tx, RequestOp::Int32Read, user);
        assert!(result.is_ok());
    }

    #[test]
    fn actor_block_unblock_process() {
        let tx = spawn_actor(TestDriver::new());

        // Write initial value
        let user = AsynUser::new(0).with_timeout(Duration::from_secs(1));
        send_and_wait(&tx, RequestOp::Int32Write { value: 10 }, user).unwrap();

        // Block with token 42
        let mut user = AsynUser::new(0).with_timeout(Duration::from_secs(1));
        user.block_token = Some(42);
        send_and_wait(&tx, RequestOp::BlockProcess, user).unwrap();

        // Owner request should succeed
        let mut user = AsynUser::new(0).with_timeout(Duration::from_secs(1));
        user.block_token = Some(42);
        send_and_wait(&tx, RequestOp::Int32Write { value: 99 }, user).unwrap();

        // Unblock (must use same token as the block holder)
        let mut user = AsynUser::new(0).with_timeout(Duration::from_secs(1));
        user.block_token = Some(42);
        send_and_wait(&tx, RequestOp::UnblockProcess, user).unwrap();

        // Non-owner should now work
        let user = AsynUser::new(0).with_timeout(Duration::from_secs(1));
        let result = send_and_wait(&tx, RequestOp::Int32Read, user).unwrap();
        assert_eq!(result.int_val, Some(99));
    }

    #[test]
    fn actor_unblock_with_expired_deadline_still_runs() {
        // An UnblockProcess that waited out its nominal I/O timeout must
        // still execute once dequeued — otherwise the port stays wedged for
        // every non-owner caller forever. No dequeued request (lifecycle or
        // I/O) is aborted by a queue deadline (C parity, asynManager.c:906).
        let mut drv = TestDriver::new();
        // Seed VAL so the post-unblock read probe returns a value rather than
        // the C-parity ParamUndefined for an unset param; this test asserts
        // the unblock lifecycle, not undefined-read state.
        drv.base.params.set_int32(0, 0, 0).unwrap();
        let tx = spawn_actor(drv);

        let mut user = AsynUser::new(0).with_timeout(Duration::from_secs(1));
        user.block_token = Some(7);
        send_and_wait(&tx, RequestOp::BlockProcess, user).unwrap();

        // Submit the unblock with an already-expired deadline.
        let mut user = AsynUser::new(0).with_timeout(Duration::from_nanos(1));
        user.block_token = Some(7);
        std::thread::sleep(Duration::from_millis(1));
        send_and_wait(&tx, RequestOp::UnblockProcess, user)
            .expect("expired-deadline UnblockProcess must still run");

        // Port must be unblocked: a non-owner request now succeeds.
        // (VAL was written above, so the read returns a defined value rather
        // than the C-parity ParamUndefined for an unset param.)
        let user = AsynUser::new(0).with_timeout(Duration::from_secs(1));
        send_and_wait(&tx, RequestOp::Int32Read, user)
            .expect("port should be unblocked after UnblockProcess");
    }

    /// C runs enable/auto-connect/get/connect/disconnect directly under
    /// asynManagerLock — never via queueRequest — so the block holder (which
    /// only gates the portThread's I/O `processUser` dispatch) cannot stall
    /// them. A non-owner's lifecycle op must complete while the port is
    /// blocked by another owner. Before the fix only UnblockProcess was exempt
    /// from the block divert, so a non-owner SetEnable/GetEnable/Connect/
    /// Disconnect sat in `pending_while_blocked` until UnblockProcess (the
    /// `send_and_wait` here would hang).
    #[test]
    fn actor_nonowner_lifecycle_runs_while_blocked() {
        let tx = spawn_actor(TestDriver::new());

        // Block the port with owner token 42.
        let mut user = AsynUser::new(0).with_timeout(Duration::from_secs(1));
        user.block_token = Some(42);
        send_and_wait(&tx, RequestOp::BlockProcess, user).unwrap();

        // A non-owner (no block_token) SetEnable(false) must run immediately,
        // not divert — otherwise this call never returns.
        let user = AsynUser::new(0).with_timeout(Duration::from_secs(1));
        send_and_wait(&tx, RequestOp::SetEnable { yes: false }, user)
            .expect("non-owner SetEnable must run while the port is blocked");

        // And it took effect while still blocked: a non-owner GetEnable
        // reflects the disable.
        let user = AsynUser::new(0).with_timeout(Duration::from_secs(1));
        let r = send_and_wait(&tx, RequestOp::GetEnable, user)
            .expect("non-owner GetEnable must run while the port is blocked");
        assert_eq!(r.int_val, Some(0), "SetEnable(false) applied while blocked");

        // The owner can still unblock cleanly (token must match).
        let mut user = AsynUser::new(0).with_timeout(Duration::from_secs(1));
        user.block_token = Some(42);
        send_and_wait(&tx, RequestOp::UnblockProcess, user).unwrap();
    }

    #[test]
    fn actor_serialization() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct CountingDriver {
            base: PortDriverBase,
            concurrent: Arc<AtomicUsize>,
            max_concurrent: Arc<AtomicUsize>,
        }

        impl PortDriver for CountingDriver {
            fn base(&self) -> &PortDriverBase {
                &self.base
            }
            fn base_mut(&mut self) -> &mut PortDriverBase {
                &mut self.base
            }
            fn io_write_int32(&mut self, _user: &mut AsynUser, value: i32) -> AsynResult<()> {
                let c = self.concurrent.fetch_add(1, Ordering::SeqCst) + 1;
                let _ = self.max_concurrent.fetch_max(c, Ordering::SeqCst);
                std::thread::sleep(Duration::from_millis(1));
                self.concurrent.fetch_sub(1, Ordering::SeqCst);
                self.base_mut().params.set_int32(0, 0, value)?;
                Ok(())
            }
        }

        let concurrent = Arc::new(AtomicUsize::new(0));
        let max_concurrent = Arc::new(AtomicUsize::new(0));
        let mut base = PortDriverBase::new("serial_actor", 1, PortFlags::default());
        base.create_param("VAL", ParamType::Int32).unwrap();
        let driver = CountingDriver {
            base,
            concurrent: concurrent.clone(),
            max_concurrent: max_concurrent.clone(),
        };

        let tx = spawn_actor(driver);

        let handles: Vec<_> = (0..20)
            .map(|i| {
                let tx = tx.clone();
                std::thread::spawn(move || {
                    let user = AsynUser::new(0).with_timeout(Duration::from_secs(5));
                    send_and_wait(&tx, RequestOp::Int32Write { value: i }, user).unwrap();
                })
            })
            .collect();

        for h in handles {
            h.join().unwrap();
        }

        assert_eq!(max_concurrent.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn actor_flush() {
        let tx = spawn_actor(TestDriver::new());
        let user = AsynUser::new(0).with_timeout(Duration::from_secs(1));
        let result = send_and_wait(&tx, RequestOp::Flush, user);
        assert!(result.is_ok());
    }

    #[test]
    fn actor_uint32_digital() {
        let mut drv = TestDriver::new();
        drv.base
            .create_param("BITS", ParamType::UInt32Digital)
            .unwrap();
        let tx = spawn_actor(drv);

        let user = AsynUser::new(4).with_timeout(Duration::from_secs(1));
        send_and_wait(
            &tx,
            RequestOp::UInt32DigitalWrite {
                value: 0xFF,
                mask: 0x0F,
            },
            user,
        )
        .unwrap();

        let user = AsynUser::new(4).with_timeout(Duration::from_secs(1));
        let result = send_and_wait(&tx, RequestOp::UInt32DigitalRead { mask: 0xFF }, user).unwrap();
        assert_eq!(result.uint_val, Some(0x0F));
    }

    #[test]
    fn actor_clean_shutdown() {
        let tx = spawn_actor(TestDriver::new());
        drop(tx); // Dropping all senders causes the actor to return
        std::thread::sleep(Duration::from_millis(10));
        // No hang, no panic
    }
}
