use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::io::{AsyncWrite, AsyncWriteExt, BufWriter};

use epics_base_rs::runtime::sync::Mutex;

use super::LongStringMode;
use super::ca_server::ServerStats;
use crate::protocol::*;
use epics_base_rs::server::event_queue::EventReader;
use epics_base_rs::server::pv::MonitorEvent;
use epics_base_rs::types::encode_dbr;

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
    sub_id: u32,
    data_type: u16,
    data_count: u32,
    writer: Arc<Mutex<BufWriter<W>>>,
    mut reader: EventReader,
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
        loop {
            // C `event_read`: take this monitor's next queued entry, suspending
            // under EVENTS_OFF exactly where C does (`flowCtrlMode &&
            // nDuplicates == 0`). The queue is the single owner of both that
            // gate and the coalescing rule — a post arriving while the ring is
            // short of room replaced this monitor's LAST entry in place, so the
            // earlier distinct entries are still here and go out as their own
            // frames, and no side slot can reorder delivery.
            let Some(event) = reader.recv().await else {
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
            let reader = pv
                .add_subscriber(1, DbFieldType::Double, DBE_VALUE)
                .await
                .expect("subscriber added");
            let stats = Arc::new(ServerStats::default());
            let task = spawn_monitor_sender(
                1,
                DBR_DOUBLE,
                0,
                recording_writer(),
                reader,
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
            let reader = pv
                .add_subscriber(1, DbFieldType::Double, DBE_VALUE)
                .await
                .expect("subscriber added");
            let stats = Arc::new(ServerStats::default());
            let task = spawn_monitor_sender(
                1,
                DBR_DOUBLE,
                0,
                recording_writer(),
                reader,
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
}
