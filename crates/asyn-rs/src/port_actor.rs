//! Actor-based port driver executor.
//!
//! Each port driver is owned exclusively by a `PortActor` task. Requests arrive
//! via an mpsc channel, are prioritized in a heap, and dispatched to the
//! driver's `io_*` methods. Replies go back through oneshot channels.
//!
//! For `can_block=true` ports, the actor runs on `tokio::task::spawn_blocking`.
//! For `can_block=false` ports, it runs on a normal `tokio::spawn` task.

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

/// Message sent from [`super::port_handle::PortHandle`] to the actor.
pub(crate) struct ActorMessage {
    pub op: RequestOp,
    pub user: AsynUser,
    pub deadline: Instant,
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
        let deadline = Instant::now() + user.timeout;
        Self {
            op,
            user,
            deadline,
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
            driver,
            rx,
            heap: BinaryHeap::new(),
            blocked_by: None,
            pending_while_blocked: Vec::new(),
        }
    }

    /// Run the actor loop. Returns when the channel is closed (all senders dropped).
    /// Calls `shutdown()` on the driver before returning.
    #[cfg(test)]
    pub fn run(mut self) {
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
    /// Returns when either:
    /// - The main request channel is closed (all senders dropped)
    /// - The shutdown channel is closed (shutdown signaled)
    pub fn run_with_shutdown(mut self, mut shutdown_rx: mpsc::Receiver<()>) {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        rt.block_on(async {
            loop {
                // Drain all pending messages into the heap
                self.drain_channel();

                if self.heap.is_empty() {
                    // Wait for either a message or shutdown
                    tokio::select! {
                        msg = self.rx.recv() => {
                            match msg {
                                Some(m) => self.enqueue_message(m),
                                None => break,
                            }
                        }
                        _ = shutdown_rx.recv() => break,
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

    fn enqueue_message(&mut self, msg: ActorMessage) {
        if let Some((owner, _)) = self.blocked_by {
            let is_owner = msg.block_token == Some(owner);
            let is_unblock = matches!(msg.op, RequestOp::UnblockProcess);
            if !is_owner && !is_unblock {
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
            deadline,
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

        let is_connect_op = matches!(
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
        );

        // Deadline check — I/O ops only. Lifecycle / queue-management ops
        // (connect, block, unblock, shutdown) are not subject to the queue
        // deadline: an UnblockProcess that waited out its deadline must
        // still run, or the port stays wedged for every non-owner caller.
        if !is_connect_op && Instant::now() > deadline {
            let _ = reply.send(Err(AsynError::Status {
                status: AsynStatus::Timeout,
                message: "request deadline expired before execution".into(),
            }));
            return;
        }
        let is_connect_priority = user.priority == QueuePriority::Connect;

        // Connect ops and Connect-priority requests bypass enabled/connected checks
        // (C parity: Connect priority processed even when disabled/disconnected)
        if !is_connect_op && !is_connect_priority {
            // Auto-connect: try to reconnect if disconnected and auto_connect is set
            if self.driver.base().flags.multi_device {
                let ds = self.driver.base().device_states.get(&user.addr);
                let dev_disconnected = !ds.map_or(true, |d| d.connected);
                let dev_auto = ds.map_or(self.driver.base().auto_connect, |d| d.auto_connect);
                if dev_disconnected && dev_auto {
                    // For multi-device, auto-connect the specific address
                    let connect_user = AsynUser::new(user.reason).with_addr(user.addr);
                    let _ = self.driver.connect_addr(&connect_user);
                }
            } else if !self.driver.base().connected && self.driver.base().auto_connect {
                let _ = self.driver.connect(&AsynUser::default());
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
                self.driver.io_write_octet(user, data)?;
                Ok(RequestResult::write_ok())
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
                res?;
                Ok(RequestResult::write_ok())
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
            RequestOp::OctetWriteRead { data, buf_size } => {
                // C parity: asynOctetSyncIO::writeRead (asynOctetSyncIO.c:250)
                // does flush() → write() → read() under a single
                // queueLockPort. The flush drains any stale bytes
                // left in the driver's input buffer from a prior
                // read so that the post-write read returns only the
                // response to *this* command (e.g. echoes from a
                // serial line, leftover prompts from a TCP device).
                // Skipping the flush leaks pre-existing input into
                // the response and breaks every command-response
                // protocol when the line was warm.
                self.driver.io_flush(user)?;
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
                        let is_unblock = matches!(msg.op, RequestOp::UnblockProcess);
                        if is_owner || is_unblock {
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
            RequestOp::DrvUserCreate { drv_info } => {
                let reason = self.driver.drv_user_create(drv_info)?;
                Ok(RequestResult::drv_user_create(reason))
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
                for u in updates {
                    match u {
                        crate::request::ParamSetValue::Int32 {
                            reason,
                            addr,
                            value,
                        } => {
                            let _ = base.set_int32_param(*reason, *addr, *value);
                        }
                        crate::request::ParamSetValue::Float64 {
                            reason,
                            addr,
                            value,
                        } => {
                            let _ = base.set_float64_param(*reason, *addr, *value);
                        }
                        crate::request::ParamSetValue::Octet {
                            reason,
                            addr,
                            value,
                        } => {
                            let _ = base.params.set_string(*reason, *addr, value.clone());
                        }
                        crate::request::ParamSetValue::Float64Array {
                            reason,
                            addr,
                            value,
                        } => {
                            let _ = base.params.set_float64_array(*reason, *addr, value.clone());
                        }
                        crate::request::ParamSetValue::Int32Array {
                            reason,
                            addr,
                            value,
                        } => {
                            let _ = base.params.set_int32_array(*reason, *addr, value.clone());
                        }
                        crate::request::ParamSetValue::UInt32Digital {
                            reason,
                            addr,
                            value,
                            mask,
                            interrupt_mask,
                        } => {
                            let _ = base.set_uint32_param(
                                *reason,
                                *addr,
                                *value,
                                *mask,
                                *interrupt_mask,
                            );
                        }
                    }
                }
                base.call_param_callbacks(*addr)?;
                Ok(RequestResult::write_ok())
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
                return Ok(r.with_alarm(alarm_status, alarm_severity, ts));
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
            fn io_write_octet(&mut self, _user: &mut AsynUser, _data: &[u8]) -> AsynResult<()> {
                *self.write_eos.lock() = self.base().output_eos.clone();
                Ok(())
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
            fn io_write_octet(&mut self, _user: &mut AsynUser, _data: &[u8]) -> AsynResult<()> {
                self.write_calls.fetch_add(1, Ordering::Relaxed);
                self.sequence.lock().push("write");
                Ok(())
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
        // C asynManager services each priority as a strict FIFO; a nearer
        // deadline must not let a later same-priority request jump ahead.
        let mk = |seq: u64, deadline: Instant| {
            let (reply, _rx) = oneshot::channel();
            ActorMessage {
                op: RequestOp::Int32Read,
                user: AsynUser::new(0),
                deadline,
                cancel: CancelToken::new(),
                reply,
                seq,
                priority: QueuePriority::Medium,
                block_token: None,
            }
        };
        let now = Instant::now();
        let mut heap = BinaryHeap::new();
        // seq 0 submitted first with a far deadline; seq 1 next with a
        // much nearer deadline.
        heap.push(mk(0, now + Duration::from_secs(100)));
        heap.push(mk(1, now + Duration::from_millis(1)));
        // FIFO: seq 0 pops first despite seq 1's nearer deadline.
        assert_eq!(heap.pop().unwrap().seq, 0);
        assert_eq!(heap.pop().unwrap().seq, 1);
    }

    #[test]
    fn heap_pops_higher_priority_first() {
        // Removing the deadline tiebreaker must not disturb priority order.
        let mk = |seq: u64, priority: QueuePriority| {
            let (reply, _rx) = oneshot::channel();
            ActorMessage {
                op: RequestOp::Int32Read,
                user: AsynUser::new(0),
                deadline: Instant::now() + Duration::from_secs(1),
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
    fn actor_deadline_expired() {
        let tx = spawn_actor(TestDriver::new());
        let user = AsynUser::new(0).with_timeout(Duration::from_nanos(1));
        std::thread::sleep(Duration::from_millis(1));
        let result = send_and_wait(&tx, RequestOp::Int32Read, user);
        match result {
            Err(AsynError::Status { status, .. }) => assert_eq!(status, AsynStatus::Timeout),
            other => panic!("expected Timeout, got {other:?}"),
        }
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
        // An UnblockProcess that waited out its queue deadline must still
        // execute — otherwise the port stays wedged for every non-owner
        // caller forever. Lifecycle ops bypass the deadline short-circuit.
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
