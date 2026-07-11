use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::io::{AsyncWrite, AsyncWriteExt, BufWriter};
use tokio::sync::Notify;

use epics_base_rs::runtime::sync::{Mutex, mpsc};

use super::LongStringMode;
use super::ca_server::ServerStats;
use crate::protocol::*;
use epics_base_rs::server::pv::{MonitorEvent, ProcessVariable, coalesce_consume};
use epics_base_rs::types::encode_dbr;

#[derive(Default)]
pub struct FlowControlGate {
    paused: AtomicBool,
    resumed: Notify,
}

impl FlowControlGate {
    pub fn pause(&self) {
        self.paused.store(true, Ordering::Release);
    }

    pub fn resume(&self) {
        self.paused.store(false, Ordering::Release);
        self.resumed.notify_waiters();
    }

    pub async fn wait_until_resumed(&self) {
        loop {
            // Register the resume waiter eagerly with `enable()` BEFORE
            // re-reading the pause flag. `Notify::notified()` does not
            // register until first polled and `notify_waiters()` stores
            // no permit, so without this a `resume()` firing between the
            // `paused.load()` and the `.await` would be lost and leave
            // this blocked until the next resume. `resume()` stores
            // `paused = false` (Release) before notifying, so if we still
            // observe `paused` here the broadcast has not fired yet and is
            // guaranteed to land on this enabled waiter. Same lost-wake-
            // safe pattern as `coalesce_while_paused` and
            // `Channel::wait_until_inactive`.
            let resumed = self.resumed.notified();
            tokio::pin!(resumed);
            resumed.as_mut().enable();
            if !self.paused.load(Ordering::Acquire) {
                return;
            }
            resumed.await;
        }
    }

    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::Acquire)
    }

    /// Collapse the monitor backlog to a single latest value while the
    /// circuit is paused (EVENTS_OFF), returning it once EVENTS_ON
    /// resumes (or `None` if the source channel closes).
    ///
    /// `pop_overflow` drains the producer's coalesce *slot* — the place
    /// `notify_subscribers` / `post_monitor` parks the newest value once
    /// the per-subscriber `rx` queue fills (`ProcessVariable` /
    /// `RecordInstance::pop_coalesced`). Both sources must be folded here:
    /// draining only `rx` lets an overflow that lands during the pause
    /// stay in the slot, so resume delivers the stale `rx` tail first and
    /// the newer slot value only on the next loop — defeating the
    /// collapse-to-latest the pause exists to provide. Mirrors the
    /// not-paused loop, which also prefers the slot as the newest value.
    pub async fn coalesce_while_paused<F, Fut>(
        &self,
        rx: &mut mpsc::Receiver<MonitorEvent>,
        mut pending: MonitorEvent,
        mut pop_overflow: F,
    ) -> Option<MonitorEvent>
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = Option<MonitorEvent>>,
    {
        loop {
            // Register the resume waiter eagerly BEFORE re-reading the
            // pause flag. `resume()` stores `paused = false` (Release)
            // *before* `notify_waiters()`, so if we still observe
            // `is_paused()` here the broadcast has not fired yet and is
            // guaranteed to land on this already-enabled waiter. This
            // closes the lost-wake gap where an EVENTS_ON arriving
            // between the recheck and the `select!` await would otherwise
            // be dropped — `Notify::notified()` does not register until
            // first polled, and `notify_waiters()` stores no permit.
            // `notify_waiters` (broadcast) is required rather than
            // `notify_one`: one circuit-level gate fans out to every
            // monitor task on the connection, and `notify_one` would wake
            // only one of them. Same pattern as
            // `Channel::wait_until_inactive`.
            let resumed = self.resumed.notified();
            tokio::pin!(resumed);
            resumed.as_mut().enable();
            if !self.is_paused() {
                break;
            }
            // Collapse the backlog to the latest value while paused:
            // drain the rx queue, then fold the producer overflow slot,
            // which holds a value newer than anything in rx once the
            // queue has filled. The slot holds at most one (overwritten)
            // value, so a single take suffices; the next wake re-drains.
            while let Ok(event) = rx.try_recv() {
                pending = event;
            }
            if let Some(event) = pop_overflow().await {
                pending = event;
            }
            if !self.is_paused() {
                break;
            }
            tokio::select! {
                maybe_event = rx.recv() => match maybe_event {
                    Some(event) => pending = event,
                    None => return None,
                },
                _ = resumed => {}
            }
        }
        Some(pending)
    }
}

/// Per-subscription view of the circuit's EVENTS_OFF gate, and the single
/// owner of the "may this update be suspended?" decision. Both monitor
/// delivery loops — `spawn_monitor_sender` and the record-field loop in
/// `tcp.rs` — run every update through [`MonitorFlow::admit`], so the two
/// cannot disagree about what a pause does.
///
/// C `event_read` (`dbEvent.c:944-950`) suspends the event task only when
/// `flowCtrlMode && nDuplicates == 0`. `nDuplicates` counts queue entries
/// beyond the first for a given monitor (`dbEvent.c:836-840`), so an update
/// still queued behind the one in hand IS a duplicate — and duplicates are
/// drained, not suspended: `event_read`'s drain loop empties the whole queue
/// without re-checking `flowCtrlMode`, so those already-queued distinct
/// updates reach the client as their own EVENT_ADD frames even while
/// EVENTS_OFF is in effect. Only once the queue is down to a single entry
/// does the next `event_read` suspend — and from then on a post *replaces*
/// that entry's value in place (`db_queue_event_log`, `dbEvent.c:808-820`)
/// rather than appending, which is the collapse-to-latest
/// [`FlowControlGate::coalesce_while_paused`] implements.
pub(crate) struct MonitorFlow {
    gate: Arc<FlowControlGate>,
    /// True while C's drain loop would still be running: it emptied the
    /// queue in one pass, so once the decision to drain is taken it is not
    /// re-taken per entry.
    draining: bool,
}

impl MonitorFlow {
    pub(crate) fn new(gate: Arc<FlowControlGate>) -> Self {
        Self {
            gate,
            draining: false,
        }
    }

    /// Returns the event to deliver, blocking until EVENTS_ON only when C
    /// would have suspended. `None` means the producer channel closed.
    pub(crate) async fn admit<F, Fut>(
        &mut self,
        rx: &mut mpsc::Receiver<MonitorEvent>,
        event: MonitorEvent,
        pop_overflow: F,
    ) -> Option<MonitorEvent>
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = Option<MonitorEvent>>,
    {
        if self.draining {
            // C's drain loop runs to `EVENTQEMPTY`; this is its last entry.
            self.draining = !rx.is_empty();
            return Some(event);
        }
        if !self.gate.is_paused() {
            return Some(event);
        }
        if !rx.is_empty() {
            // npend >= 2 ⇒ nDuplicates > 0 ⇒ C does not suspend: it drains
            // the queue, this entry included.
            self.draining = true;
            return Some(event);
        }
        // npend == 1 ⇒ nDuplicates == 0 ⇒ C suspends holding this entry,
        // and every post arriving during the pause replaces its value.
        self.gate
            .coalesce_while_paused(rx, event, pop_overflow)
            .await
    }
}

/// Spawn a task that forwards monitor events from a PV subscription to the client TCP stream.
/// Returns a handle that can be used to cancel the subscription.
///
/// Generic over the writer type so the same task body works for plain
/// `tokio::net::tcp::OwnedWriteHalf` and the TLS-wrapped
/// `WriteHalf<TlsStream<TcpStream>>` produced by the server's TLS
/// dispatch path.
/// `data_count` is the original EVENT_ADD request count. When
/// non-zero, every monitor delivery echoes this in the header and
/// zero-pads short payloads up to `dbr_buffer_size(type, native,
/// count)` — matches C `read_reply` which keeps the request count
/// and pads (or uses `snapshot.value.count()` when the request was
/// autosize=0).
// `long_string_mode` propagated from `ChannelEntry`; monitor events are
// converted inside `send_event` per the channel's mode — `$`-suffix
// channels `EpicsValue::String` → `EpicsValue::CharArray[40]` (C
// dbChannel.c:483-507), long-string record fields `EpicsValue::CharArray`
// → scalar `EpicsValue::String` (C cvt_dbaddr).
#[allow(clippy::too_many_arguments)]
pub(crate) fn spawn_monitor_sender<W>(
    pv: Arc<ProcessVariable>,
    sub_id: u32,
    data_type: u16,
    data_count: u32,
    writer: Arc<Mutex<BufWriter<W>>>,
    flow_control: Arc<FlowControlGate>,
    mut rx: mpsc::Receiver<MonitorEvent>,
    denied: Arc<AtomicBool>,
    long_string_mode: LongStringMode,
    stats: Option<Arc<ServerStats>>,
    // The EVENT_ADD request header (C `pevext->msg`) plus the client's
    // negotiated minor version. Every delivery on this subscription is framed
    // for THIS client: a pre-CA_V49 peer gets `ECA_16KARRAYCLIENT` rather than
    // a 24-byte extended header it cannot parse (`caserverio.c:266-270`).
    reply: super::tcp::ReplyContext,
) -> tokio::task::JoinHandle<()>
where
    W: AsyncWrite + Unpin + Send + 'static,
{
    epics_base_rs::runtime::task::spawn(async move {
        let mut flow = MonitorFlow::new(flow_control);
        loop {
            // Block on the queue front, then fold the producer's coalesce
            // overflow slot. When the queue filled while we were busy the
            // newest value is parked in the slot; `coalesce_consume`
            // delivers it AND drains the now-stale queue tail, so delivery
            // never steps from the newest value back to an older queued one.
            // A set slot implies the queue was full, so the front `recv()`
            // returns immediately — no added latency over checking the slot
            // first, and no newest-then-old replay of the stale backlog.
            let Some(queued) = rx.recv().await else { break };
            let coalesced = pv.pop_coalesced(sub_id).await;
            let event = coalesce_consume(&mut rx, queued, coalesced);
            // EVENTS_OFF decision — see `MonitorFlow`.
            let Some(event) = flow
                .admit(&mut rx, event, || pv.pop_coalesced(sub_id))
                .await
            else {
                break;
            };
            // One subscription update committed for delivery this cycle
            // (post-coalesce). PCAS `subscriptionEventsPosted` parity —
            // see `ServerStats::subscription_events_posted`. Counted
            // before the read-access gate so a suppressed delivery shows
            // as posted-but-not-processed, exactly as the gateway's
            // `serverPostRate` > `serverEventRate` divergence expects.
            if let Some(ref s) = stats {
                s.subscription_events_posted.fetch_add(1, Ordering::Relaxed);
            }
            // C `casAccessRightsCB` (`rsrv/camessage.c:1080-1095`)
            // suppresses event deliveries with `db_event_disable`
            // while read access is denied (without tearing the
            // subscription down). Producer keeps running so a
            // later re-enable resumes the same camonitor; we just
            // drop the event silently.
            if denied.load(Ordering::Acquire) {
                continue;
            }
            if send_event(
                data_type,
                data_count,
                sub_id,
                &event,
                &writer,
                long_string_mode,
                reply,
            )
            .await
            .is_err()
            {
                break;
            }
            // Successfully written to the client — PCAS
            // `subscriptionEventsProcessed` parity (gateway
            // `serverEventRate`).
            if let Some(ref s) = stats {
                s.subscription_events_processed
                    .fetch_add(1, Ordering::Relaxed);
            }
        }
    })
}

async fn send_event<W: AsyncWrite + Unpin + Send + 'static>(
    data_type: u16,
    data_count: u32,
    sub_id: u32,
    event: &MonitorEvent,
    writer: &Arc<Mutex<BufWriter<W>>>,
    long_string_mode: LongStringMode,
    reply: super::tcp::ReplyContext,
) -> std::io::Result<()> {
    let (req_hdr, client_minor) = (&reply.req_hdr, reply.client_minor);
    // Apply the channel's long-string boundary conversion before
    // encoding (`$` → CHAR[40]+NUL, or a long-string record field →
    // scalar DBR_STRING). Clone only when a conversion actually runs
    // (most channels are `Plain`).
    let ls_snap;
    let snapshot = if long_string_mode == LongStringMode::Plain {
        &event.snapshot
    } else {
        ls_snap = {
            let mut s = event.snapshot.clone();
            super::apply_long_string_mode(&mut s, long_string_mode);
            s
        };
        &ls_snap
    };
    let mut payload = encode_dbr(data_type, snapshot)
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidData, "encode"))?;
    // CA-268: DBR_CLASS_NAME wire payload is always one fixed 40-byte
    // string regardless of the underlying value count. Same override
    // already applied at the GET / send_monitor_snapshot / RecordField
    // event loop sites. SimplePv channels carry no record_type, so
    // class_name stays None and the body is 40 zero bytes — matches
    // IOC behaviour for synthetic channels.
    //
    // when the EVENT_ADD request set an explicit count, every
    // monitor delivery echoes that count and zero-pads the payload up
    // to `dbr_buffer_size(type, native, count)` (C `read_reply`
    // `rsrv/camessage.c:507-571` parity). The helper returns the
    // header count to use; `data_count == 0` means autosize (use the
    // live snapshot count).
    // Enforce request count in BOTH directions —
    // pad when requested > actual AND truncate when requested <
    // actual. C `read_reply` (`rsrv/camessage.c:507-571`) sizes
    // the payload to `dbr_size_n(type, request_count)` either way.
    let actual_count = snapshot.value.count() as u32;
    let element_count = if data_type == epics_base_rs::types::DBR_CLASS_NAME {
        1
    } else if data_count == 0 {
        actual_count
    } else if let Ok(native) = epics_base_rs::types::native_type_for_dbr(data_type) {
        let meta_size = epics_base_rs::types::dbr_buffer_size(data_type, native, 0);
        let target_size = meta_size + (data_count as usize) * native.element_size();
        if data_count > actual_count {
            let cur = payload.len();
            if cur < target_size {
                payload.extend(std::iter::repeat_n(0u8, target_size - cur));
            }
        } else if data_count < actual_count && payload.len() > target_size {
            payload.truncate(target_size);
        }
        data_count
    } else {
        data_count
    };
    let mut padded = payload;
    padded.resize(align8(padded.len()), 0);

    let mut hdr = CaHeader::new(CA_PROTO_EVENT_ADD);
    // C client TCP parser requires 8-byte aligned postsize. C `read_reply`
    // (`camessage.c:515-524`): when `cas_copy_in_header` refuses the frame
    // because this client is pre-CA_V49, the update is answered with
    // CA_PROTO_ERROR / ECA_16KARRAYCLIENT and the circuit is kept.
    if hdr
        .set_payload_size(padded.len(), element_count, client_minor)
        .is_err()
    {
        let _ =
            super::tcp::send_16k_array_client_err(writer, req_hdr, req_hdr.cid, client_minor).await;
        return Ok(());
    }
    hdr.data_type = data_type;
    hdr.cid = 1; // ECA_NORMAL status
    hdr.available = sub_id;

    // Abort-safety: this runs inside a monitor task that
    // `handle_client` may `task.abort()` (EVENT_CANCEL / CLEAR_CHANNEL
    // / disconnect cleanup). `tokio::abort()` drops the task at the
    // next await point. If the header and payload were written in two
    // separate `write_all` awaits, an abort landing between them would
    // leave an orphan header in the shared BufWriter, mis-framing every
    // subsequent message the next lock holder ships. Build the whole
    // CA_PROTO_EVENT_ADD frame as ONE contiguous buffer and issue a
    // single `write_all`, so an abort can only land at a frame boundary
    // (before or after the complete write), never mid-frame. The flush
    // stays separate: an aborted flush merely leaves whole frames
    // buffered, which the next lock holder flushes — harmless.
    let hdr_bytes = hdr.to_bytes_extended();
    let mut frame = Vec::with_capacity(hdr_bytes.len() + padded.len());
    frame.extend_from_slice(&hdr_bytes);
    frame.extend_from_slice(&padded);
    let mut w = writer.lock().await;
    w.write_all(&frame).await?;
    w.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::pin::Pin;
    use std::task::{Context, Poll};

    /// Mock `AsyncWrite` that records the length of every `poll_write`
    /// batch it receives. Wrapped in a zero-capacity `BufWriter`, each
    /// `write_all` is forwarded straight through (tokio's `BufWriter`
    /// bypasses its buffer when the input is at least as large as the
    /// buffer capacity), so the recorded batches map 1:1 to the
    /// `write_all` calls `send_event` issues.
    #[derive(Default)]
    struct RecordingWriter {
        /// One entry per `poll_write` batch — the bytes delivered.
        batches: Vec<Vec<u8>>,
    }

    impl AsyncWrite for RecordingWriter {
        fn poll_write(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            self.batches.push(buf.to_vec());
            Poll::Ready(Ok(buf.len()))
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    /// Abort-safety regression: `send_event` must emit the CA_PROTO_EVENT_ADD
    /// header and payload as ONE contiguous `write_all`. A split across two
    /// `write_all` awaits would let a `task.abort()` land between them,
    /// leaving an orphan header in the shared `BufWriter` and mis-framing
    /// every subsequent message. A true abort-race is non-deterministic to
    /// schedule, so this asserts the structural property that makes the
    /// race impossible: exactly one write batch, equal to the full frame.
    #[tokio::test]
    async fn send_event_writes_frame_in_single_write_all() {
        use epics_base_rs::server::pv::MonitorEvent;
        use epics_base_rs::server::snapshot::Snapshot;
        use epics_base_rs::types::{DBR_LONG, EpicsValue};

        // Zero-capacity BufWriter: every write_all forwards directly to the
        // RecordingWriter, so batch count == write_all count.
        let writer = Arc::new(Mutex::new(BufWriter::with_capacity(
            0,
            RecordingWriter::default(),
        )));

        let snapshot = Snapshot::new(
            EpicsValue::Long(42),
            0,
            0,
            std::time::SystemTime::UNIX_EPOCH,
        );
        let event = MonitorEvent {
            snapshot,
            origin: 0,
            mask: epics_base_rs::server::recgbl::EventMask::VALUE,
        };

        // data_count = 0 means autosize (use snapshot's actual count);
        // matches every producer caller.
        send_event(
            DBR_LONG,
            0,
            7,
            &event,
            &writer,
            LongStringMode::Plain,
            crate::server::tcp::ReplyContext {
                req_hdr: crate::protocol::CaHeader::new(crate::protocol::CA_PROTO_EVENT_ADD),
                client_minor: crate::protocol::CA_MINOR_VERSION,
            },
        )
        .await
        .expect("send_event must succeed");

        let guard = writer.lock().await;
        let batches = &guard.get_ref().batches;

        // Exactly one write batch — header and payload are not split.
        assert_eq!(
            batches.len(),
            1,
            "send_event must issue exactly one write_all (got {} batches: {:?})",
            batches.len(),
            batches.iter().map(|b| b.len()).collect::<Vec<_>>(),
        );

        let frame = &batches[0];

        // A single scalar DBR_LONG (4 bytes -> 8 padded, count 1) stays
        // under the 0xFFFF extended-header threshold, so the frame is a
        // standard 16-byte header followed by the padded payload. The
        // single batch must be exactly that complete frame.
        assert!(
            frame.len() >= 16,
            "frame shorter than a CA header: {} bytes",
            frame.len(),
        );
        let payload_size = u16::from_be_bytes([frame[2], frame[3]]) as usize;
        assert_ne!(
            payload_size, 0xFFFF,
            "test value unexpectedly produced an extended header",
        );
        assert_eq!(
            16 + payload_size,
            frame.len(),
            "header-declared payload size ({payload_size}) plus header (16) \
             must equal the contiguous frame length ({})",
            frame.len(),
        );
        // Payload is 8-byte aligned (C client TCP parser requirement).
        assert_eq!(payload_size % 8, 0, "payload not 8-byte aligned");
    }

    /// Server-wide subscription-event counters (PCAS
    /// `subscriptionEventsPosted` / `subscriptionEventsProcessed`,
    /// feeding the CA gateway's `serverPostRate` / `serverEventRate`).
    /// The monitor task is the single owner that advances them, so the
    /// invariant boundaries are tested here against the real delivery
    /// loop rather than through a full TCP round-trip.
    mod subscription_event_counters {
        use super::*;
        use crate::server::ca_server::ServerStats;
        use epics_base_rs::server::pv::ProcessVariable;
        use epics_base_rs::types::{DBR_DOUBLE, DbFieldType, EpicsValue};
        use std::time::Duration;

        const DBE_VALUE: u16 = 1;

        fn recording_writer() -> Arc<Mutex<BufWriter<RecordingWriter>>> {
            Arc::new(Mutex::new(BufWriter::with_capacity(
                0,
                RecordingWriter::default(),
            )))
        }

        /// Poll `counter` until it reaches `want`, driving the
        /// single-threaded test runtime so the spawned monitor task can
        /// run. Fails the test if it does not arrive within the timeout.
        async fn wait_for(counter: &std::sync::atomic::AtomicU64, want: u64) {
            tokio::time::timeout(Duration::from_secs(5), async {
                while counter.load(Ordering::Relaxed) < want {
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
            })
            .await
            .unwrap_or_else(|_| {
                panic!(
                    "counter reached {} but wanted {want}",
                    counter.load(Ordering::Relaxed)
                )
            });
        }

        /// Successful delivery advances posted AND processed in lockstep:
        /// every dequeued event is posted, then — with read access
        /// granted and the writer always succeeding — processed. Each
        /// post is drained before the next so coalescing never collapses
        /// two updates into one cycle, giving a deterministic count.
        #[tokio::test]
        async fn successful_delivery_advances_posted_and_processed() {
            let pv = Arc::new(ProcessVariable::new("c:pv".into(), EpicsValue::Double(0.0)));
            let rx = pv
                .add_subscriber(1, DbFieldType::Double, DBE_VALUE)
                .await
                .expect("subscriber added");
            let stats = Arc::new(ServerStats::default());
            let task = spawn_monitor_sender(
                pv.clone(),
                1,
                DBR_DOUBLE,
                0,
                recording_writer(),
                Arc::new(FlowControlGate::default()),
                rx,
                Arc::new(AtomicBool::new(false)),
                LongStringMode::Plain,
                Some(stats.clone()),
                crate::server::tcp::ReplyContext {
                    req_hdr: crate::protocol::CaHeader::new(crate::protocol::CA_PROTO_EVENT_ADD),
                    client_minor: crate::protocol::CA_MINOR_VERSION,
                },
            );

            for (i, v) in [1.0_f64, 2.0, 3.0].into_iter().enumerate() {
                pv.set(EpicsValue::Double(v)).await;
                wait_for(&stats.subscription_events_processed, i as u64 + 1).await;
            }

            task.abort();
            assert_eq!(
                stats.subscription_events_processed.load(Ordering::Relaxed),
                3,
                "three drained value updates → three processed events"
            );
            assert_eq!(
                stats.subscription_events_posted.load(Ordering::Relaxed),
                3,
                "posted matches processed when no event is suppressed or fails"
            );
        }

        /// Read access denied: the producer keeps running and the task
        /// keeps dequeuing events (so they are POSTED), but each is
        /// dropped before the wire — so PROCESSED never advances. This
        /// is the `serverPostRate` > `serverEventRate` divergence the
        /// gateway surfaces; both counters reading equal would hide it.
        #[tokio::test]
        async fn denied_delivery_counts_posted_not_processed() {
            let pv = Arc::new(ProcessVariable::new("c:pv".into(), EpicsValue::Double(0.0)));
            let rx = pv
                .add_subscriber(1, DbFieldType::Double, DBE_VALUE)
                .await
                .expect("subscriber added");
            let stats = Arc::new(ServerStats::default());
            let task = spawn_monitor_sender(
                pv.clone(),
                1,
                DBR_DOUBLE,
                0,
                recording_writer(),
                Arc::new(FlowControlGate::default()),
                rx,
                Arc::new(AtomicBool::new(true)), // read access denied
                LongStringMode::Plain,
                Some(stats.clone()),
                crate::server::tcp::ReplyContext {
                    req_hdr: crate::protocol::CaHeader::new(crate::protocol::CA_PROTO_EVENT_ADD),
                    client_minor: crate::protocol::CA_MINOR_VERSION,
                },
            );

            pv.set(EpicsValue::Double(1.0)).await;
            wait_for(&stats.subscription_events_posted, 1).await;
            // Give the task ample opportunity to (wrongly) process it.
            for _ in 0..16 {
                tokio::task::yield_now().await;
            }

            task.abort();
            assert!(
                stats.subscription_events_posted.load(Ordering::Relaxed) >= 1,
                "a denied event is still posted to the subscription"
            );
            assert_eq!(
                stats.subscription_events_processed.load(Ordering::Relaxed),
                0,
                "a denied event is suppressed before the wire — never processed"
            );
        }
    }

    /// FlowControlGate (EVENTS_OFF/EVENTS_ON) pause/resume boundaries.
    /// The gate is the single owner of the monitor pause transition that
    /// both `spawn_monitor_sender` and the record-field monitor loop
    /// acquire through `coalesce_while_paused`. Tested by boundary
    /// (paused vs not at entry, backlog squash, channel open vs closed,
    /// resume wake) rather than by narrative scenario.
    mod flow_control_gate {
        use super::*;
        use epics_base_rs::server::pv::MonitorEvent;
        use epics_base_rs::server::snapshot::Snapshot;
        use epics_base_rs::types::EpicsValue;

        fn ev(v: i32) -> MonitorEvent {
            MonitorEvent {
                snapshot: Snapshot::new(
                    EpicsValue::Long(v),
                    0,
                    0,
                    std::time::SystemTime::UNIX_EPOCH,
                ),
                origin: 0,
                mask: epics_base_rs::server::recgbl::EventMask::VALUE,
            }
        }

        fn value_of(e: &MonitorEvent) -> i32 {
            match e.snapshot.value {
                EpicsValue::Long(v) => v,
                ref other => panic!("expected Long, got {other:?}"),
            }
        }

        /// Not paused at entry → the pending value is returned at once,
        /// without waiting on any rx event or resume.
        #[tokio::test]
        async fn not_paused_at_entry_returns_pending_immediately() {
            let gate = FlowControlGate::default();
            // No sender is ever used; default gate is not paused.
            let (_tx, mut rx) = mpsc::channel::<MonitorEvent>(1);
            let got = gate
                .coalesce_while_paused(&mut rx, ev(7), || async { None })
                .await;
            assert_eq!(value_of(&got.expect("returns pending")), 7);
        }

        /// Paused → backlog squashes to the latest into `pending`; the
        /// resume flushes only that latest value (no per-event frame).
        #[tokio::test]
        async fn coalesce_to_latest_while_paused_then_resume_flushes() {
            let gate = Arc::new(FlowControlGate::default());
            gate.pause();
            let (tx, mut rx) = mpsc::channel::<MonitorEvent>(8);
            let g2 = gate.clone();
            let task = epics_base_rs::runtime::task::spawn(async move {
                g2.coalesce_while_paused(&mut rx, ev(1), || async { None })
                    .await
            });
            // Feed newer values while paused; each yield lets the gate
            // absorb the value into `pending` and park again.
            for v in [2i32, 3, 4] {
                tx.send(ev(v)).await.unwrap();
                tokio::task::yield_now().await;
            }
            gate.resume();
            let got = task.await.unwrap().expect("resume delivers latest");
            assert_eq!(
                value_of(&got),
                4,
                "coalesce yields only the latest value on resume"
            );
        }

        /// Paused + the producer overflow slot holds the genuine newest
        /// value (rx queue filled during the pause) → resume must deliver
        /// that slot value, NOT the stale rx tail. Regression for the
        /// pause coalescing only draining rx: previously the rx tail went
        /// out first and the newer slot value only on the next loop.
        #[tokio::test]
        async fn overflow_slot_folds_into_latest_on_resume() {
            let gate = Arc::new(FlowControlGate::default());
            gate.pause();
            let (tx, mut rx) = mpsc::channel::<MonitorEvent>(8);
            // The rx backlog is older than the overflow slot.
            tx.send(ev(2)).await.unwrap();
            tx.send(ev(3)).await.unwrap();
            // Producer parked the genuine newest value in the slot once
            // rx filled. `Cell::take` yields it once, then `None`.
            let slot = std::cell::Cell::new(Some(ev(99)));
            // Resume from a separate task (touches only the Send gate
            // Arc) after the coalesce future has drained + folded the
            // slot and parked. The coalesce future itself holds the
            // non-Send `Cell`, so it runs on this task, not spawned.
            let g2 = gate.clone();
            let resumer = epics_base_rs::runtime::task::spawn(async move {
                tokio::task::yield_now().await;
                g2.resume();
            });
            let got = gate
                .coalesce_while_paused(&mut rx, ev(1), || async { slot.take() })
                .await
                .expect("resume delivers the coalesced latest");
            resumer.await.unwrap();
            assert_eq!(
                value_of(&got),
                99,
                "overflow slot (newest) must win over the stale rx tail"
            );
        }

        /// Paused + the source channel closes → `None` (caller ends the
        /// subscription), not a hang.
        #[tokio::test]
        async fn channel_close_while_paused_returns_none() {
            let gate = Arc::new(FlowControlGate::default());
            gate.pause();
            let (tx, mut rx) = mpsc::channel::<MonitorEvent>(1);
            let g2 = gate.clone();
            let task = epics_base_rs::runtime::task::spawn(async move {
                g2.coalesce_while_paused(&mut rx, ev(1), || async { None })
                    .await
            });
            tokio::task::yield_now().await; // let the consumer park in the select
            drop(tx); // closes the channel; rx.recv() resolves to None
            assert!(
                task.await.unwrap().is_none(),
                "channel close while paused yields None"
            );
        }

        /// A resume delivered while the consumer is parked must wake it
        /// and flush the held value WITHOUT requiring a further rx event.
        /// The held value is the only one ever sent, so a lost resume
        /// wake would strand the consumer forever.
        #[tokio::test]
        async fn resume_flushes_held_without_further_event() {
            let gate = Arc::new(FlowControlGate::default());
            gate.pause();
            let (tx, mut rx) = mpsc::channel::<MonitorEvent>(4);
            let g2 = gate.clone();
            let task = epics_base_rs::runtime::task::spawn(async move {
                g2.coalesce_while_paused(&mut rx, ev(1), || async { None })
                    .await
            });
            // One value arrives during the pause, then the source goes
            // quiet — only a resume can release the consumer.
            tx.send(ev(5)).await.unwrap();
            tokio::task::yield_now().await;
            gate.resume();
            let got = tokio::time::timeout(std::time::Duration::from_secs(2), task)
                .await
                .expect("resume must wake a parked consumer (no lost wake)")
                .unwrap()
                .expect("resume delivers the held value");
            assert_eq!(value_of(&got), 5);
        }

        /// R8-21 boundary: a backlog that was ALREADY queued when EVENTS_OFF
        /// arrived must be drained — each queued distinct update goes out as
        /// its own frame, even while paused. C `event_read`
        /// (`dbEvent.c:944-950`) suspends only when `nDuplicates == 0`, and
        /// an update queued behind the one in hand is a duplicate; the drain
        /// loop then empties the whole queue without re-checking
        /// `flowCtrlMode`. Pre-fix Rust collapsed the three updates into one
        /// latest-value frame.
        #[tokio::test]
        async fn r8_21_prepause_backlog_is_drained_not_collapsed() {
            let gate = Arc::new(FlowControlGate::default());
            let (tx, mut rx) = mpsc::channel::<MonitorEvent>(8);
            // Three updates queued BEFORE the pause: one in hand + two in rx.
            tx.send(ev(2)).await.unwrap();
            tx.send(ev(3)).await.unwrap();
            gate.pause();

            let mut flow = MonitorFlow::new(gate.clone());
            let mut delivered = Vec::new();
            // First update is the one already dequeued by the task loop.
            let mut in_hand = Some(ev(1));
            for _ in 0..3 {
                let event = match in_hand.take() {
                    Some(e) => e,
                    None => rx.recv().await.expect("queued update"),
                };
                // Every admit must return without waiting for EVENTS_ON —
                // the gate is still paused for the whole loop.
                let got = tokio::time::timeout(
                    std::time::Duration::from_secs(2),
                    flow.admit(&mut rx, event, || async { None }),
                )
                .await
                .expect("a queued duplicate must not suspend the subscription")
                .expect("channel open");
                delivered.push(value_of(&got));
            }
            assert!(gate.is_paused(), "the drain happens while EVENTS_OFF holds");
            assert_eq!(
                delivered,
                vec![1, 2, 3],
                "each already-queued distinct update goes out as its own frame"
            );
        }

        /// The other side of the same boundary: once the queue is down to a
        /// single entry (`nDuplicates == 0`) C suspends, and posts arriving
        /// during the pause REPLACE that entry's value
        /// (`db_queue_event_log`, `dbEvent.c:808-820`) instead of appending.
        /// So a during-pause burst still collapses to one latest-value frame.
        #[tokio::test]
        async fn r8_21_posts_during_the_pause_still_collapse_to_latest() {
            let gate = Arc::new(FlowControlGate::default());
            gate.pause();
            let (tx, mut rx) = mpsc::channel::<MonitorEvent>(8);
            let g2 = gate.clone();
            let task = epics_base_rs::runtime::task::spawn(async move {
                let mut flow = MonitorFlow::new(g2);
                flow.admit(&mut rx, ev(1), || async { None }).await
            });
            // Let the subscription park on the held entry FIRST — that is what
            // makes the posts below "arrive during the pause" (C: npend == 1,
            // nDuplicates == 0, so each post replaces the queued entry).
            tokio::task::yield_now().await;
            for v in [2i32, 3] {
                tx.send(ev(v)).await.unwrap();
                tokio::task::yield_now().await;
            }
            gate.resume();
            let got = task.await.unwrap().expect("resume delivers the held entry");
            assert_eq!(
                value_of(&got),
                3,
                "during-pause posts replace the held entry — one frame, latest value"
            );
        }

        /// After the pre-pause backlog has drained, the subscription is back
        /// to `nDuplicates == 0` and the NEXT update must suspend again — the
        /// drain is one pass over the queue, not a permanent bypass of the
        /// pause.
        #[tokio::test]
        async fn r8_21_drain_does_not_disable_the_pause_for_later_updates() {
            let gate = Arc::new(FlowControlGate::default());
            let (tx, mut rx) = mpsc::channel::<MonitorEvent>(8);
            tx.send(ev(2)).await.unwrap();
            gate.pause();

            let mut flow = MonitorFlow::new(gate.clone());
            // Backlog of two (one in hand + one queued) drains.
            let first = flow
                .admit(&mut rx, ev(1), || async { None })
                .await
                .expect("open");
            assert_eq!(value_of(&first), 1);
            let queued = rx.recv().await.unwrap();
            let second = flow
                .admit(&mut rx, queued, || async { None })
                .await
                .expect("open");
            assert_eq!(value_of(&second), 2, "the last queued entry is drained too");

            // Queue is empty now: the next update must be held until resume.
            tx.send(ev(9)).await.unwrap();
            let queued = rx.recv().await.unwrap();
            let held = tokio::time::timeout(
                std::time::Duration::from_millis(200),
                flow.admit(&mut rx, queued, || async { None }),
            )
            .await;
            assert!(
                held.is_err(),
                "with the queue drained, a further update must suspend under EVENTS_OFF"
            );
        }

        /// Not paused → `admit` is a pass-through (no drain state, no wait).
        #[tokio::test]
        async fn admit_passes_through_when_not_paused() {
            let gate = Arc::new(FlowControlGate::default());
            let (_tx, mut rx) = mpsc::channel::<MonitorEvent>(1);
            let mut flow = MonitorFlow::new(gate);
            let got = flow
                .admit(&mut rx, ev(7), || async { None })
                .await
                .expect("open");
            assert_eq!(value_of(&got), 7);
        }

        /// `wait_until_resumed` (used by external callers of the gate)
        /// must return after a resume even when the resume races the
        /// consumer's entry — the eager `enable()` closes the same
        /// lost-wake gap as `coalesce_while_paused`.
        #[tokio::test]
        async fn wait_until_resumed_unblocks_on_resume() {
            let gate = Arc::new(FlowControlGate::default());
            gate.pause();
            let g2 = gate.clone();
            let task =
                epics_base_rs::runtime::task::spawn(async move { g2.wait_until_resumed().await });
            tokio::task::yield_now().await; // let it park
            gate.resume();
            tokio::time::timeout(std::time::Duration::from_secs(2), task)
                .await
                .expect("wait_until_resumed must return after resume")
                .unwrap();
        }
    }
}
