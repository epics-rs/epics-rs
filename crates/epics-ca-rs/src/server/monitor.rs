use std::borrow::Cow;
use std::sync::atomic::Ordering;

use super::frame::size_dbr_reply;
use super::outbox::{Credit, FrameSink, Outbox};
use crate::protocol::*;
pub(crate) use epics_base_rs::server::database::dbr_delivery::{Update, with_live_options};
use epics_base_rs::server::pv::MonitorEvent;

/// Deliver one queued entry of a `SimplePv` monitor — the `user_sub` C's
/// `event_read` calls for it.
pub(crate) async fn deliver_monitor_event(
    m: &super::tcp::CircuitMonitor,
    event: MonitorEvent,
    // The single per-connection wire outbox — the connection loop is the sole
    // writer owner. See `super::outbox`.
    outbox: &Outbox,
    // The circuit's minor as this delivery read it.
    client_minor: u16,
) {
    // One subscription update committed for delivery this cycle
    // (post-coalesce). PCAS `subscriptionEventsPosted` parity —
    // see `ServerStats::subscription_events_posted`. Counted
    // before the read-access gate so a suppressed delivery shows
    // as posted-but-not-processed, exactly as the gateway's
    // `serverPostRate` > `serverEventRate` divergence expects.
    if let Some(ref s) = m.stats {
        s.subscription_events_posted.fetch_add(1, Ordering::Relaxed);
    }
    if m.denied.load(Ordering::Acquire) {
        m.gate
            .deliver(|| super::tcp::send_denied_event(m, outbox, client_minor));
        return;
    }
    // Back-pressure, taken once this event is known to become a frame:
    // after the access gate, so a suppressed delivery spends none
    // (`super::outbox` invariant). With the outbox full this parks the
    // circuit's event task, and every later post then coalesces in the ring
    // instead of queueing — which is what stops a client that has stopped
    // reading from growing the server without limit. C's `event_task`
    // blocks in the socket send the same way.
    let credit = outbox.reserve().await;
    let mut read = None;
    let Some(update) = with_live_options(m.data_type, Cow::Owned(event), &mut read, || {
        m.live_options()
    }) else {
        super::tcp::send_get_fail_event(
            outbox,
            m.refusal_channel(),
            m.data_type,
            m.data_count,
            m.sub_id,
            m.reply(client_minor),
        );
        return;
    };
    // C `db_cancel_event` nulls `user_sub` under LOCKEVQUE before
    // `event_cancel_reply` writes the delete-confirmed echo, so this
    // frame either precedes the echo or never exists. `deliver` is
    // that decision: the enqueue happens inside it, and the teardown
    // revokes before it queues the echo. `None` = cancelled while this
    // delivery was between its ring and the outbox.
    let delivered = m.gate.deliver(|| {
        send_event(
            m.refusal_channel(),
            m.data_type,
            m.data_count,
            m.sub_id,
            update,
            outbox,
            credit,
            &m.stage,
            client_minor,
            &m.req_hdr,
        )
    });
    if delivered.is_none() {
        return;
    }
    // Successfully written to the client — PCAS
    // `subscriptionEventsProcessed` parity (gateway
    // `serverEventRate`).
    if let Some(ref s) = m.stats {
        s.subscription_events_processed
            .fetch_add(1, Ordering::Relaxed);
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn send_event(
    chan: super::tcp::RefusalChannel<'_>,
    data_type: u16,
    data_count: u32,
    sub_id: u32,
    update: Update<'_>,
    outbox: &impl FrameSink,
    // Spent on the delivery below. `Credit::none()` from callers that already
    // run under the connection loop's own bound — see `super::outbox`.
    credit: Credit,
    stage: &super::DeliveryStage,
    client_minor: u16,
    // Read only when the update is refused.
    req_hdr: &CaHeader,
) {
    if super::tcp::refuse_unframeable_update(
        outbox,
        chan,
        data_type,
        data_count,
        stage,
        client_minor,
        req_hdr,
    ) {
        return;
    }
    // The channel's post chain and long-string boundary conversion
    // (`$` → CHAR[40]+NUL, or a long-string record field → scalar
    // DBR_STRING).
    let mut log = None;
    let Some(delivery) = update.prepare(stage, data_count, &mut log) else {
        return;
    };
    // The header space reserved and the payload encoded straight into it,
    // in the client's send buffer where the sink has one — C
    // `cas_copy_in_header` reserving inside it and `dbGet` converting into
    // that space (`camessage.c:511` → `dbAccess.c:1017-1018`). See
    // `server::frame`.
    let count = super::frame::reply_count(data_type, delivery.count(), data_count);
    let payload = super::frame::payload_size(data_type, count);
    let hdr_len = super::frame::header_len(payload, count);
    let len_hint = hdr_len + payload;
    // Why the frame was dropped, answered once it is out of the buffer.
    let mut unsent = None;
    outbox.frame_with(hdr_len, len_hint, credit, |frame| {
        if delivery.encode_dbr_into(frame.dst(), data_type).is_err() {
            unsent = Some(Unsent::GetFail);
            return None;
        }
        // Size the payload and pick the header count — `size_dbr_reply` owns
        // the rule (autosize, request-count pad/truncate, and the
        // DBR_CLASS_NAME fixed single element). A SimplePv channel carries no
        // record_type, so its CLASS_NAME body is 40 zero bytes, matching an
        // IOC's synthetic channels.
        let element_count = size_dbr_reply(frame, data_type, delivery.count(), data_count);
        frame.align_payload();

        let mut hdr = CaHeader::new(CA_PROTO_EVENT_ADD);
        // C client TCP parser requires 8-byte aligned postsize. C `read_reply`
        // (`camessage.c:515-524`): when `cas_copy_in_header` refuses the frame
        // because this client is pre-CA_V49, the update is answered with
        // CA_PROTO_ERROR / ECA_16KARRAYCLIENT and the circuit is kept.
        if hdr
            .set_payload_size(frame.payload_len(), element_count, client_minor)
            .is_err()
        {
            unsent = Some(Unsent::Refused(element_count));
            return None;
        }
        hdr.data_type = data_type;
        hdr.cid = 1; // ECA_NORMAL status
        hdr.available = sub_id;
        Some(hdr)
    });
    match unsent {
        None => {}
        Some(Unsent::GetFail) => super::tcp::send_get_fail_event(
            outbox,
            chan,
            data_type,
            data_count,
            sub_id,
            super::tcp::ReplyContext {
                req_hdr: *req_hdr,
                client_minor,
            },
        ),
        Some(Unsent::Refused(element_count)) => {
            let _ = super::tcp::send_16k_array_client_err(
                outbox,
                req_hdr,
                chan,
                super::tcp::UnloadedReply::ReadReply,
                element_count,
                client_minor,
            );
        }
    }
}

/// Why [`send_event`] built no frame.
enum Unsent {
    GetFail,
    Refused(u32),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::outbox::{Outbox, OutboxDrain};
    use epics_base_rs::server::snapshot::Snapshot;
    use epics_base_rs::types::EpicsValue;

    /// Collect every frame currently queued in the outbox, in push order.
    /// Each `push` is one complete frame, so the returned vec maps 1:1 to
    /// the frames a producer emitted — the socket-free equivalent of the
    /// old "one `write_all` batch per frame" assertion.
    fn drain_frames(drain: &mut OutboxDrain) -> Vec<Vec<u8>> {
        let mut frames = Vec::new();
        while let Some(frame) = drain.try_next() {
            frames.push(frame.to_vec());
        }
        frames
    }

    /// Abort-safety regression: `send_event` must emit the CA_PROTO_EVENT_ADD
    /// header and payload as ONE contiguous frame pushed with a single
    /// `Outbox::push`. A split into two pushes would let a `task.abort()`
    /// land between them, enqueuing an orphan header that mis-frames every
    /// following message. A true abort-race is non-deterministic to
    /// schedule, so this asserts the structural property that makes the
    /// race impossible: exactly one queued frame, equal to the full frame.
    #[epics_macros_rs::epics_test]
    async fn send_event_pushes_frame_in_single_push() {
        use epics_base_rs::server::pv::MonitorEvent;
        use epics_base_rs::server::snapshot::Snapshot;
        use epics_base_rs::types::{DBR_LONG, EpicsValue};

        let (outbox, mut drain): (Outbox, OutboxDrain) = crate::server::outbox::channel();

        let mut snapshot = Snapshot::new(
            EpicsValue::Long(42),
            0,
            0,
            std::time::SystemTime::UNIX_EPOCH,
        );
        let event = MonitorEvent::read_log(
            &mut snapshot,
            epics_base_rs::server::recgbl::EventMask::VALUE,
        );

        // data_count = 0 means autosize (use snapshot's actual count);
        // matches every producer caller.
        send_event(
            crate::server::tcp::RefusalChannel::UNNAMED,
            DBR_LONG,
            0,
            7,
            Update::log(Cow::Owned(event)),
            &outbox,
            Credit::none(),
            &crate::server::DeliveryStage::plain(),
            crate::protocol::CA_MINOR_VERSION,
            &crate::protocol::CaHeader::new(crate::protocol::CA_PROTO_EVENT_ADD),
        );

        let batches = drain_frames(&mut drain);

        // Exactly one queued frame — header and payload are not split.
        assert_eq!(
            batches.len(),
            1,
            "send_event must push exactly one frame (got {} frames: {:?})",
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

    /// A GR/CTRL update encodes the channel read, which the post chain runs
    /// on in place of the log (`prepare_snapshot`); a post filter that drops
    /// it sends nothing, as C's `dbChannelRunPostChain` returning NULL does.
    #[epics_macros_rs::epics_test]
    async fn a_read_update_the_post_chain_drops_sends_no_frame() {
        use epics_base_rs::server::database::filters::{
            FilterChain, FilteredMonitorEvent, SubscriptionFilter,
        };
        use epics_base_rs::types::{DBR_CTRL_DOUBLE, EpicsValue};
        struct Post(bool);
        impl SubscriptionFilter for Post {
            fn name(&self) -> &'static str {
                "post"
            }
            fn apply(&self, event: FilteredMonitorEvent) -> Option<FilteredMonitorEvent> {
                self.0.then_some(event)
            }
            fn post_chain(&self) -> bool {
                true
            }
        }
        for pass in [true, false] {
            let mut chain = FilterChain::new();
            chain.push(std::sync::Arc::new(Post(pass)));
            let stage =
                crate::server::DeliveryStage::new(chain, crate::server::LongStringMode::Plain, 1);
            let (outbox, mut drain): (Outbox, OutboxDrain) = crate::server::outbox::channel();
            let mut read = Snapshot::new(
                EpicsValue::Double(1.5),
                0,
                0,
                std::time::SystemTime::UNIX_EPOCH,
            );
            send_event(
                crate::server::tcp::RefusalChannel::UNNAMED,
                DBR_CTRL_DOUBLE,
                0,
                7,
                Update::of_read(&mut read),
                &outbox,
                Credit::none(),
                &stage,
                crate::protocol::CA_MINOR_VERSION,
                &crate::protocol::CaHeader::new(crate::protocol::CA_PROTO_EVENT_ADD),
            );
            assert_eq!(
                drain_frames(&mut drain).len(),
                usize::from(pass),
                "pass {pass}"
            );
        }
    }

    /// Server-wide subscription-event counters (PCAS
    /// `subscriptionEventsPosted` / `subscriptionEventsProcessed`,
    /// feeding the CA gateway's `serverPostRate` / `serverEventRate`).
    /// The circuit's event task is the single owner that advances them, so the
    /// invariant boundaries are tested here against the real delivery
    /// loop rather than through a full TCP round-trip.
    mod subscription_event_counters {
        use super::*;
        use crate::server::stats::ServerStats;
        use epics_base_rs::server::pv::ProcessVariable;
        use epics_base_rs::types::{DBR_DOUBLE, DbFieldType, EpicsValue};
        use std::sync::Arc;
        use std::sync::atomic::AtomicBool;
        use std::time::Duration;

        use crate::server::outbox::{Outbox, OutboxDrain};

        const DBE_VALUE: u16 = 1;

        /// A live outbox whose drain the caller keeps bound for the test's
        /// duration, so the circuit event task's `push`es are accepted
        /// (an already-dropped drain would silently discard them — still
        /// correct for the counter assertions, but keeping the drain alive
        /// models a live connection).
        fn live_outbox() -> (Outbox, OutboxDrain) {
            crate::server::outbox::channel()
        }

        /// One `SimplePv` monitor on its own circuit, delivered by the
        /// circuit's event task. The reader is the dispatch side's handle:
        /// dropping it cancels the monitor.
        fn start_circuit(
            pv: &Arc<ProcessVariable>,
            denied: bool,
            stats: &Arc<ServerStats>,
            outbox: Outbox,
        ) -> (
            epics_base_rs::runtime::task::TaskHandle<()>,
            epics_base_rs::server::event_queue::EventReader,
        ) {
            let user = Arc::new(epics_base_rs::server::event_queue::EventUser::new());
            let context: epics_base_rs::server::event_queue::DeliveryContext =
                Arc::new(crate::server::tcp::CircuitMonitor {
                    target: crate::server::tcp::ChannelTarget::SimplePv(pv.clone()),
                    db: Arc::new(epics_base_rs::server::database::PvDatabase::new()),
                    cid: 0,
                    sub_id: 1,
                    data_type: DBR_DOUBLE,
                    data_count: 0,
                    denied: AtomicBool::new(denied),
                    gate: crate::server::outbox::MonitorGate::open(),
                    stage: crate::server::DeliveryStage::plain(),
                    req_hdr: crate::protocol::CaHeader::new(crate::protocol::CA_PROTO_EVENT_ADD),
                    stats: Some(stats.clone()),
                    ended: AtomicBool::new(false),
                });
            let reader = pv
                .add_subscriber_on(&user, 1, DbFieldType::Double, DBE_VALUE, &context)
                .expect("subscriber added");
            let reactor = epics_base_rs::runtime::task::Reactor::current()
                .expect("the test driver enters an executor");
            let task = reactor.spawn(crate::server::tcp::run_circuit_monitors(
                user,
                outbox,
                crate::server::tcp::CircuitMinor::new(crate::protocol::CA_MINOR_VERSION),
            ));
            (task, reader)
        }

        /// Poll `counter` until it reaches `want`, driving the
        /// single-threaded test runtime so the circuit event task can
        /// run. Fails the test if it does not arrive within the timeout.
        async fn wait_for(counter: &std::sync::atomic::AtomicU64, want: u64) {
            epics_base_rs::runtime::task::timeout(Duration::from_secs(5), async {
                while counter.load(Ordering::Relaxed) < want {
                    epics_base_rs::runtime::task::sleep(Duration::from_millis(1)).await;
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
        #[epics_macros_rs::epics_test]
        async fn successful_delivery_advances_posted_and_processed() {
            let pv = Arc::new(ProcessVariable::new("c:pv", EpicsValue::Double(0.0)));
            let stats = Arc::new(ServerStats::default());
            let (outbox, _drain) = live_outbox();
            let (task, _reader) = start_circuit(&pv, false, &stats, outbox);

            for (i, v) in [1.0_f64, 2.0, 3.0].into_iter().enumerate() {
                pv.set(EpicsValue::Double(v));
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
        #[epics_macros_rs::epics_test]
        async fn denied_delivery_counts_posted_not_processed() {
            let pv = Arc::new(ProcessVariable::new("c:pv", EpicsValue::Double(0.0)));
            let stats = Arc::new(ServerStats::default());
            let (outbox, _drain) = live_outbox();
            let (task, _reader) = start_circuit(&pv, true, &stats, outbox); // read access denied

            pv.set(EpicsValue::Double(1.0));
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

    /// A log delivered alone counts and encodes as the log itself does,
    /// whether its value is a scalar copied in or a boxed array.
    #[test]
    fn a_log_alone_encodes_as_the_log_does() {
        use epics_base_rs::server::recgbl::EventMask;
        for value in [
            EpicsValue::Double(2.5),
            EpicsValue::DoubleArray(vec![1.0, 2.0, 3.0].into()),
        ] {
            let posted = Snapshot::new(value, 3, 1, std::time::UNIX_EPOCH);
            for data_type in 0..=20u16 {
                let event = || MonitorEvent::post_log(&posted, 0, EventMask::VALUE);
                let mut want = Vec::new();
                let want_res = event().encode_dbr_into(&mut want, data_type);
                let want_count = event().value().as_value().map_or(0, |v| v.count());
                // A class that reads an option has no log-alone delivery.
                let mut read = None;
                let alone = with_live_options(data_type, Cow::Owned(event()), &mut read, || None);
                for update in std::iter::once(Update::log(Cow::Owned(event()))).chain(alone) {
                    let mut log = None;
                    let delivery = update
                        .prepare(&crate::server::DeliveryStage::plain(), 0, &mut log)
                        .expect("an identity stage drops nothing");
                    assert!(delivery.read().is_none(), "type {data_type}");
                    let mut got = Vec::new();
                    let got_res = delivery.encode_dbr_into(&mut got, data_type);
                    assert_eq!(got_res.is_ok(), want_res.is_ok(), "type {data_type}");
                    assert_eq!(got, want, "type {data_type}");
                    assert_eq!(delivery.count(), want_count, "type {data_type}");
                }
            }
        }
    }
}
