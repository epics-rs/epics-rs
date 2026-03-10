//! Request types and completion mechanism for the port worker queue.
//!
//! This module provides the request/response primitives used by [`crate::port_worker::PortWorker`]
//! to serialize I/O operations through a per-port worker thread.

use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering as AtomicOrdering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant, SystemTime};

use crate::error::{AsynError, AsynResult, AsynStatus};
use crate::port::QueuePriority;
use crate::user::AsynUser;

/// Global sequence counter for FIFO ordering within the same priority+deadline.
static SEQ_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Operation the worker thread will dispatch to the port driver.
#[derive(Debug, Clone)]
pub enum RequestOp {
    OctetWrite { data: Vec<u8> },
    OctetRead { buf_size: usize },
    OctetWriteRead { data: Vec<u8>, buf_size: usize },
    Int32Write { value: i32 },
    Int32Read,
    Int64Write { value: i64 },
    Int64Read,
    Float64Write { value: f64 },
    Float64Read,
    UInt32DigitalWrite { value: u32, mask: u32 },
    UInt32DigitalRead { mask: u32 },
    Flush,
    /// Connect to the port (bypass enabled/connected checks).
    Connect,
    /// Disconnect from the port (bypass enabled/connected checks).
    Disconnect,
    /// Block the port: only this user's requests will be dequeued until unblocked.
    BlockProcess,
    /// Unblock the port.
    UnblockProcess,
    /// Resolve a driver info string to a parameter reason index.
    DrvUserCreate { drv_info: String },
    /// Read an enum value (index + string choices).
    EnumRead,
    /// Write an enum index.
    EnumWrite { index: usize },
    /// Read an i32 array.
    Int32ArrayRead { max_elements: usize },
    /// Write an i32 array.
    Int32ArrayWrite { data: Vec<i32> },
    /// Read an f64 array.
    Float64ArrayRead { max_elements: usize },
    /// Write an f64 array.
    Float64ArrayWrite { data: Vec<f64> },
    /// Flush changed parameters as interrupt notifications (callParamCallbacks).
    CallParamCallbacks { addr: i32 },
}

/// Result returned by the worker after executing a request.
#[derive(Debug)]
pub struct RequestResult {
    pub status: AsynStatus,
    pub message: String,
    pub nbytes: usize,
    pub data: Option<Vec<u8>>,
    pub int_val: Option<i32>,
    pub int64_val: Option<i64>,
    pub float_val: Option<f64>,
    pub uint_val: Option<u32>,
    /// Reason index (from DrvUserCreate).
    pub reason: Option<usize>,
    /// Enum index (from EnumRead).
    pub enum_index: Option<usize>,
    /// i32 array data (from Int32ArrayRead).
    pub int32_array: Option<Vec<i32>>,
    /// f64 array data (from Float64ArrayRead).
    pub float64_array: Option<Vec<f64>>,
    /// Alarm status from the driver param store (populated on reads).
    pub alarm_status: u16,
    /// Alarm severity from the driver param store (populated on reads).
    pub alarm_severity: u16,
    /// Timestamp from the driver param store (populated on reads).
    pub timestamp: Option<SystemTime>,
}

impl RequestResult {
    fn base() -> Self {
        Self {
            status: AsynStatus::Success,
            message: String::new(),
            nbytes: 0,
            data: None,
            int_val: None,
            int64_val: None,
            float_val: None,
            uint_val: None,
            reason: None,
            enum_index: None,
            int32_array: None,
            float64_array: None,
            alarm_status: 0,
            alarm_severity: 0,
            timestamp: None,
        }
    }

    pub fn write_ok() -> Self {
        Self::base()
    }

    pub fn octet_read(buf: Vec<u8>, nbytes: usize) -> Self {
        Self { nbytes, data: Some(buf), ..Self::base() }
    }

    pub fn int32_read(value: i32) -> Self {
        Self { int_val: Some(value), ..Self::base() }
    }

    pub fn int64_read(value: i64) -> Self {
        Self { int64_val: Some(value), ..Self::base() }
    }

    pub fn float64_read(value: f64) -> Self {
        Self { float_val: Some(value), ..Self::base() }
    }

    pub fn uint32_read(value: u32) -> Self {
        Self { uint_val: Some(value), ..Self::base() }
    }

    pub fn drv_user_create(reason: usize) -> Self {
        Self { reason: Some(reason), ..Self::base() }
    }

    pub fn enum_read(index: usize) -> Self {
        Self { enum_index: Some(index), ..Self::base() }
    }

    pub fn int32_array_read(data: Vec<i32>) -> Self {
        Self { int32_array: Some(data), ..Self::base() }
    }

    pub fn float64_array_read(data: Vec<f64>) -> Self {
        Self { float64_array: Some(data), ..Self::base() }
    }

    /// Attach alarm/timestamp metadata to this result.
    pub fn with_alarm(mut self, alarm_status: u16, alarm_severity: u16, timestamp: Option<SystemTime>) -> Self {
        self.alarm_status = alarm_status;
        self.alarm_severity = alarm_severity;
        self.timestamp = timestamp;
        self
    }
}

/// Token for cancelling a queued request before execution.
#[derive(Clone, Debug)]
pub struct CancelToken(pub Arc<AtomicBool>);

impl CancelToken {
    pub fn new() -> Self {
        Self(Arc::new(AtomicBool::new(false)))
    }

    pub fn cancel(&self) {
        self.0.store(true, AtomicOrdering::Release);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(AtomicOrdering::Acquire)
    }
}

impl Default for CancelToken {
    fn default() -> Self {
        Self::new()
    }
}

/// Internal state for completion signaling.
struct CompletionState {
    done: bool,
    result: Option<AsynResult<RequestResult>>,
}

/// Handle for waiting on request completion. Uses std::sync primitives (no tokio dependency).
#[derive(Clone)]
pub struct CompletionHandle {
    inner: Arc<(Mutex<CompletionState>, Condvar)>,
}

impl CompletionHandle {
    pub fn new() -> Self {
        Self {
            inner: Arc::new((
                Mutex::new(CompletionState {
                    done: false,
                    result: None,
                }),
                Condvar::new(),
            )),
        }
    }

    /// Called by the worker thread to signal completion. Duplicate calls are ignored.
    pub fn complete(&self, result: AsynResult<RequestResult>) {
        let (lock, cvar) = &*self.inner;
        let mut state = lock.lock().unwrap();
        if state.done {
            return; // already completed
        }
        state.result = Some(result);
        state.done = true;
        cvar.notify_all();
    }

    /// Block until completion or timeout. Returns the result or `AsynStatus::Timeout`.
    pub fn wait(&self, timeout: Duration) -> AsynResult<RequestResult> {
        let (lock, cvar) = &*self.inner;
        let mut state = lock.lock().unwrap();
        let deadline = Instant::now() + timeout;
        while !state.done {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(AsynError::Status {
                    status: AsynStatus::Timeout,
                    message: "completion wait timed out".into(),
                });
            }
            let (new_state, wait_result) = cvar.wait_timeout(state, remaining).unwrap();
            state = new_state;
            if wait_result.timed_out() && !state.done {
                return Err(AsynError::Status {
                    status: AsynStatus::Timeout,
                    message: "completion wait timed out".into(),
                });
            }
        }
        state
            .result
            .take()
            .expect("completion signaled done but result is None")
    }
}

impl Default for CompletionHandle {
    fn default() -> Self {
        Self::new()
    }
}

/// A request queued in the priority queue, ready for the worker thread.
pub(crate) struct QueuedRequest {
    pub seq: u64,
    pub priority: QueuePriority,
    pub op: RequestOp,
    pub user: AsynUser,
    pub deadline: Instant,
    pub cancel: CancelToken,
    pub completion: CompletionHandle,
    /// Token identifying the user that submitted this request (for BlockProcess filtering).
    pub block_token: Option<u64>,
}

impl QueuedRequest {
    /// Create a new queued request with auto-incrementing sequence number.
    pub fn new(
        op: RequestOp,
        user: AsynUser,
        deadline: Instant,
        cancel: CancelToken,
        completion: CompletionHandle,
    ) -> Self {
        let block_token = user.block_token;
        Self {
            seq: SEQ_COUNTER.fetch_add(1, AtomicOrdering::Relaxed),
            priority: user.priority,
            op,
            user,
            deadline,
            cancel,
            completion,
            block_token,
        }
    }
}

// BinaryHeap is a max-heap. We want: highest priority first, then nearest deadline, then lowest seq.
impl Eq for QueuedRequest {}

impl PartialEq for QueuedRequest {
    fn eq(&self, other: &Self) -> bool {
        self.seq == other.seq
    }
}

impl Ord for QueuedRequest {
    fn cmp(&self, other: &Self) -> Ordering {
        // Higher priority = larger enum value = should come first (max-heap natural)
        self.priority
            .cmp(&other.priority)
            // Nearer deadline should come first → reverse (farther deadline = smaller in max-heap)
            .then_with(|| other.deadline.cmp(&self.deadline))
            // Lower seq should come first → reverse (larger seq = smaller in max-heap)
            .then_with(|| other.seq.cmp(&self.seq))
    }
}

impl PartialOrd for QueuedRequest {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Priority queue wrapper used internally by [`crate::port_worker`].
pub(crate) struct RequestHeap {
    heap: BinaryHeap<QueuedRequest>,
}

impl RequestHeap {
    pub fn new() -> Self {
        Self {
            heap: BinaryHeap::new(),
        }
    }

    pub fn push(&mut self, req: QueuedRequest) {
        self.heap.push(req);
    }

    pub fn pop(&mut self) -> Option<QueuedRequest> {
        self.heap.pop()
    }

    pub fn peek(&self) -> Option<&QueuedRequest> {
        self.heap.peek()
    }

    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.heap.is_empty()
    }

    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.heap.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn completion_roundtrip() {
        let handle = CompletionHandle::new();
        let h2 = handle.clone();

        let t = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(10));
            h2.complete(Ok(RequestResult::write_ok()));
        });

        let result = handle.wait(Duration::from_secs(1)).unwrap();
        assert_eq!(result.status, AsynStatus::Success);
        t.join().unwrap();
    }

    #[test]
    fn completion_timeout() {
        let handle = CompletionHandle::new();
        let err = handle.wait(Duration::from_millis(10)).unwrap_err();
        match err {
            AsynError::Status { status, .. } => assert_eq!(status, AsynStatus::Timeout),
            _ => panic!("expected Timeout, got {err:?}"),
        }
    }

    #[test]
    fn cancel_token() {
        let token = CancelToken::new();
        assert!(!token.is_cancelled());
        token.cancel();
        assert!(token.is_cancelled());
    }

    #[test]
    fn completion_double_complete_ignored() {
        let handle = CompletionHandle::new();
        handle.complete(Ok(RequestResult::write_ok()));
        // Second complete is silently ignored
        handle.complete(Ok(RequestResult::int32_read(42)));
        let result = handle.wait(Duration::from_millis(10)).unwrap();
        // Should get the first result
        assert_eq!(result.status, AsynStatus::Success);
        assert!(result.int_val.is_none()); // write_ok, not int32_read
    }

    #[test]
    fn queue_ordering_priority() {
        let mut heap = RequestHeap::new();
        let now = Instant::now() + Duration::from_secs(10);

        // Low priority first
        let mut user_low = AsynUser::default();
        user_low.priority = QueuePriority::Low;
        heap.push(QueuedRequest::new(
            RequestOp::Flush,
            user_low,
            now,
            CancelToken::new(),
            CompletionHandle::new(),
        ));

        // High priority second
        let mut user_high = AsynUser::default();
        user_high.priority = QueuePriority::High;
        heap.push(QueuedRequest::new(
            RequestOp::Flush,
            user_high,
            now,
            CancelToken::new(),
            CompletionHandle::new(),
        ));

        // Medium priority third
        let mut user_med = AsynUser::default();
        user_med.priority = QueuePriority::Medium;
        heap.push(QueuedRequest::new(
            RequestOp::Flush,
            user_med,
            now,
            CancelToken::new(),
            CompletionHandle::new(),
        ));

        // Should come out: High, Medium, Low
        assert_eq!(heap.pop().unwrap().priority, QueuePriority::High);
        assert_eq!(heap.pop().unwrap().priority, QueuePriority::Medium);
        assert_eq!(heap.pop().unwrap().priority, QueuePriority::Low);
    }

    #[test]
    fn queue_ordering_deadline_tiebreak() {
        let mut heap = RequestHeap::new();
        let base = Instant::now();

        // Same priority, farther deadline
        let user1 = AsynUser::default();
        heap.push(QueuedRequest::new(
            RequestOp::Flush,
            user1,
            base + Duration::from_secs(10),
            CancelToken::new(),
            CompletionHandle::new(),
        ));

        // Same priority, nearer deadline
        let user2 = AsynUser::default();
        heap.push(QueuedRequest::new(
            RequestOp::Flush,
            user2,
            base + Duration::from_secs(1),
            CancelToken::new(),
            CompletionHandle::new(),
        ));

        // Nearer deadline should come first
        let first = heap.pop().unwrap();
        let second = heap.pop().unwrap();
        assert!(first.deadline < second.deadline);
    }

    #[test]
    fn queue_ordering_fifo_seq() {
        let mut heap = RequestHeap::new();
        let deadline = Instant::now() + Duration::from_secs(10);

        // Same priority, same deadline — FIFO by seq
        let user1 = AsynUser::default();
        let req1 = QueuedRequest::new(
            RequestOp::Int32Read,
            user1,
            deadline,
            CancelToken::new(),
            CompletionHandle::new(),
        );
        let seq1 = req1.seq;
        heap.push(req1);

        let user2 = AsynUser::default();
        let req2 = QueuedRequest::new(
            RequestOp::Float64Read,
            user2,
            deadline,
            CancelToken::new(),
            CompletionHandle::new(),
        );
        let seq2 = req2.seq;
        heap.push(req2);

        assert!(seq1 < seq2);
        // Lower seq (first enqueued) should come out first
        assert_eq!(heap.pop().unwrap().seq, seq1);
        assert_eq!(heap.pop().unwrap().seq, seq2);
    }
}
