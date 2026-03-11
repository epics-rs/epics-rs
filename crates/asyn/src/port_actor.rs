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

// Heap ordering: higher priority first, then nearer deadline, then lower seq (FIFO)
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
            .then_with(|| other.deadline.cmp(&self.deadline))
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
    blocked_by: Option<u64>,
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
    #[cfg(test)]
    pub fn run(mut self) {
        loop {
            // Drain all pending messages into the heap
            self.drain_channel();

            if self.heap.is_empty() {
                // No work — block on the channel
                match self.rx.blocking_recv() {
                    Some(msg) => self.enqueue_message(msg),
                    None => return,
                }
                // Drain any more that arrived
                self.drain_channel();
            }

            // Process one eligible request from the heap
            self.process_one();
        }
    }

    /// Run the actor loop with a dedicated shutdown channel.
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
                                None => return,
                            }
                        }
                        _ = shutdown_rx.recv() => return,
                    }
                    // Drain any more that arrived
                    self.drain_channel();
                }

                // Process one eligible request from the heap
                self.process_one();
            }
        });
    }

    fn drain_channel(&mut self) {
        while let Ok(msg) = self.rx.try_recv() {
            self.enqueue_message(msg);
        }
    }

    fn enqueue_message(&mut self, msg: ActorMessage) {
        if let Some(owner) = self.blocked_by {
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

        // Cancel check
        if cancel.is_cancelled() {
            let _ = reply.send(Err(AsynError::Status {
                status: AsynStatus::Error,
                message: "request cancelled".into(),
            }));
            return;
        }

        // Deadline check
        if Instant::now() > deadline {
            let _ = reply.send(Err(AsynError::Status {
                status: AsynStatus::Timeout,
                message: "request deadline expired before execution".into(),
            }));
            return;
        }

        let is_connect_op = matches!(op, RequestOp::Connect | RequestOp::Disconnect);

        if !is_connect_op {
            // Auto-connect
            let should_auto = if self.driver.base().flags.multi_device {
                let ds = self.driver.base().device_states.get(&user.addr);
                !ds.map_or(true, |d| d.connected)
                    && ds.map_or(self.driver.base().auto_connect, |d| d.auto_connect)
            } else {
                !self.driver.base().connected && self.driver.base().auto_connect
            };
            if should_auto {
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
                | RequestOp::OctetWriteRead { .. }
                | RequestOp::UInt32DigitalRead { .. }
                | RequestOp::EnumRead
                | RequestOp::Int32ArrayRead { .. }
                | RequestOp::Float64ArrayRead { .. }
        );

        let result = match op {
            RequestOp::OctetWrite { data } => {
                self.driver.io_write_octet(user, data)?;
                Ok(RequestResult::write_ok())
            }
            RequestOp::OctetRead { buf_size } => {
                let mut buf = vec![0u8; *buf_size];
                let n = self.driver.io_read_octet(user, &mut buf)?;
                buf.truncate(n);
                Ok(RequestResult::octet_read(buf, n))
            }
            RequestOp::OctetWriteRead { data, buf_size } => {
                self.driver.io_write_octet(user, data)?;
                let mut buf = vec![0u8; *buf_size];
                let n = self.driver.io_read_octet(user, &mut buf)?;
                buf.truncate(n);
                Ok(RequestResult::octet_read(buf, n))
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
            RequestOp::BlockProcess => {
                let token = user.block_token.unwrap_or(user.reason as u64);
                self.blocked_by = Some(token);
                Ok(RequestResult::write_ok())
            }
            RequestOp::UnblockProcess => {
                self.blocked_by = None;
                let pending = std::mem::take(&mut self.pending_while_blocked);
                for msg in pending {
                    self.heap.push(msg);
                }
                Ok(RequestResult::write_ok())
            }
            RequestOp::DrvUserCreate { drv_info } => {
                let reason = self.driver.drv_user_create(drv_info)?;
                Ok(RequestResult::drv_user_create(reason))
            }
            RequestOp::EnumRead => {
                let (idx, _entries) = self.driver.read_enum(user)?;
                Ok(RequestResult::enum_read(idx))
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
            RequestOp::CallParamCallbacks { addr } => {
                self.driver.base_mut().call_param_callbacks(*addr)?;
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
        };

        // Attach alarm/timestamp metadata on successful reads
        if is_read {
            if let Ok(r) = result {
                let (_, alarm_status, alarm_severity) = self
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
                return Ok(r.with_alarm(alarm_status, alarm_severity, ts));
            }
        }

        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::param::ParamType;
    use crate::port::{PortDriverBase, PortFlags};
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
        let result = send_and_wait(
            &tx,
            RequestOp::OctetRead { buf_size: 256 },
            user,
        )
        .unwrap();
        assert_eq!(&result.data.unwrap()[..5], b"hello");
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
        let tx = spawn_actor(drv);
        let user = AsynUser::new(0).with_timeout(Duration::from_secs(1));
        let result = send_and_wait(&tx, RequestOp::Int32Read, user);
        assert!(result.is_ok());
    }

    #[test]
    fn actor_connect_disconnect() {
        let tx = spawn_actor(TestDriver::new());

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

        // Unblock
        let user = AsynUser::new(0).with_timeout(Duration::from_secs(1));
        send_and_wait(&tx, RequestOp::UnblockProcess, user).unwrap();

        // Non-owner should now work
        let user = AsynUser::new(0).with_timeout(Duration::from_secs(1));
        let result = send_and_wait(&tx, RequestOp::Int32Read, user).unwrap();
        assert_eq!(result.int_val, Some(99));
    }

    #[test]
    fn actor_serialization() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

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
            fn io_write_int32(
                &mut self,
                _user: &mut AsynUser,
                value: i32,
            ) -> AsynResult<()> {
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
        drv.base.create_param("BITS", ParamType::UInt32Digital).unwrap();
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
        let result = send_and_wait(
            &tx,
            RequestOp::UInt32DigitalRead { mask: 0xFF },
            user,
        )
        .unwrap();
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
