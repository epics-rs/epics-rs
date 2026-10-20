//! [`SendBuf`] at its boundaries, and the CAS-event thread's real sink
//! ([`EventSendBuf`]) seen from the wire: one write per batch of ready
//! frames, whichever arm of [`run_event_task`] staged them.

use std::future::Future;
use std::pin::pin;
use std::task::{Context, Poll, Waker};

use epics_base_rs::runtime::task::block_on_sync;
use epics_base_rs::server::event_queue::{EventUser, no_context};
use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::types::{DbFieldType, EpicsValue};

use super::*;
use crate::protocol::MAX_TCP;
use std::sync::atomic::AtomicUsize;

/// A socket that records each write call, or refuses every one once broken.
#[derive(Default)]
struct Wire {
    writes: Mutex<Vec<Vec<u8>>>,
    broken: AtomicBool,
}

impl Write for &Wire {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if self.broken.load(Ordering::Relaxed) {
            return Err(std::io::ErrorKind::BrokenPipe.into());
        }
        self.writes.lock().unwrap().push(bytes.to_vec());
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Wire {
    fn writes(&self) -> Vec<Vec<u8>> {
        self.writes.lock().unwrap().clone()
    }
}

fn send_buf() -> (Arc<Wire>, SendBuf<Wire>) {
    let wire = Arc::new(Wire::default());
    (wire.clone(), SendBuf::new(wire))
}

fn frame(tag: u8, len: usize) -> Vec<u8> {
    vec![tag; len]
}

#[test]
fn a_frame_that_exactly_fills_the_buffer_stays_staged() {
    let (wire, mut sb) = send_buf();
    sb.stage(&frame(1, 100)).unwrap();
    sb.stage(&frame(2, MAX_TCP - 100)).unwrap();
    assert!(wire.writes().is_empty(), "a full buffer is not yet a send");
    sb.send().unwrap();
    assert_eq!(
        wire.writes(),
        [[frame(1, 100), frame(2, MAX_TCP - 100)].concat()]
    );
}

#[test]
fn a_frame_that_does_not_fit_sends_the_staged_bytes_first() {
    let (wire, mut sb) = send_buf();
    sb.stage(&frame(1, MAX_TCP - 10)).unwrap();
    sb.stage(&frame(2, 11)).unwrap();
    assert_eq!(wire.writes(), [frame(1, MAX_TCP - 10)]);
    sb.send().unwrap();
    assert_eq!(wire.writes(), [frame(1, MAX_TCP - 10), frame(2, 11)]);
}

#[test]
fn a_frame_larger_than_the_buffer_follows_the_staged_bytes_on_the_wire() {
    let (wire, mut sb) = send_buf();
    sb.stage(&frame(1, 16)).unwrap();
    sb.stage(&frame(2, MAX_TCP + 1)).unwrap();
    assert_eq!(wire.writes(), [frame(1, 16), frame(2, MAX_TCP + 1)]);
    sb.send().unwrap();
    assert_eq!(wire.writes().len(), 2, "the large frame was never staged");
}

#[test]
fn a_failed_send_reports_it_and_drops_what_was_staged() {
    let (wire, mut sb) = send_buf();
    sb.stage(&frame(1, 16)).unwrap();
    wire.broken.store(true, Ordering::Relaxed);
    assert!(sb.send().is_err());
    wire.broken.store(false, Ordering::Relaxed);
    sb.stage(&frame(2, 16)).unwrap();
    sb.send().unwrap();
    assert_eq!(wire.writes(), [frame(2, 16)]);
}

#[test]
fn a_stage_whose_flush_fails_reports_it_and_stages_nothing() {
    let (wire, mut sb) = send_buf();
    sb.stage(&frame(1, MAX_TCP)).unwrap();
    wire.broken.store(true, Ordering::Relaxed);
    assert!(sb.stage(&frame(2, 16)).is_err());
    wire.broken.store(false, Ordering::Relaxed);
    sb.send().unwrap();
    assert!(wire.writes().is_empty());
}

/// `stage_with` keeps `stage`'s boundaries: a frame expected not to fit
/// sends what is staged first, and one larger than the buffer goes out on
/// its own.
#[test]
fn a_frame_built_in_place_keeps_the_staging_boundaries() {
    let (wire, mut sb) = send_buf();
    sb.stage(&frame(1, MAX_TCP - 10)).unwrap();
    sb.stage_with(11, |mut b| {
        b.extend(frame(2, 11));
        b
    })
    .unwrap();
    assert_eq!(wire.writes(), [frame(1, MAX_TCP - 10)]);
    sb.stage_with(MAX_TCP + 1, |mut b| {
        b.extend(frame(3, MAX_TCP + 1));
        b
    })
    .unwrap();
    assert_eq!(
        wire.writes(),
        [frame(1, MAX_TCP - 10), frame(2, 11), frame(3, MAX_TCP + 1)]
    );
    sb.stage_with(16, |b| b).unwrap();
    sb.send().unwrap();
    assert_eq!(wire.writes().len(), 3, "a dropped frame stages nothing");
}

/// A sink that builds every frame apart and records it — the copy path
/// `stage_with`'s default takes.
#[derive(Clone, Default)]
struct Copies(Arc<Mutex<Vec<u8>>>);

impl crate::server::tcp::EventSink for Copies {
    fn stage(&mut self, frame: &[u8]) -> std::io::Result<()> {
        self.0.lock().unwrap().extend_from_slice(frame);
        Ok(())
    }
    fn send(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Monitor updates built straight into the send buffer reach the wire as
/// the same bytes, in the same order, as frames built apart and copied in.
#[test]
fn updates_built_in_place_are_the_frames_built_apart() {
    const N: u32 = 5;
    let run = |sink: &mut dyn FnMut(
        Arc<EventUser>,
        mpsc::UnboundedReceiver<EventTaskControl>,
        &[Arc<epics_base_rs::server::pv::ProcessVariable>],
    )| {
        let user = Arc::new(EventUser::new());
        let (ctl_tx, ctl_rx) = mpsc::unbounded_channel();
        let pvs = circuit_monitored_pvs(N, &user, &ctl_tx);
        sink(user, ctl_rx, &pvs);
        drop(ctl_tx);
    };
    let post = |pvs: &[Arc<epics_base_rs::server::pv::ProcessVariable>]| {
        for (i, pv) in pvs.iter().enumerate().rev() {
            pv.set(EpicsValue::Double(i as f64 + 0.5));
        }
    };
    let wire = Arc::new(Wire::default());
    run(&mut |user, ctl_rx, pvs| {
        let sink = EventSendBuf(Arc::new(Mutex::new(SendBuf::new(wire.clone()))));
        let mut task = pin!(run_event_task(
            user,
            ctl_rx,
            sink,
            crate::server::tcp::CircuitMinor::new(CA_MINOR_VERSION)
        ));
        assert!(poll_once(task.as_mut()).is_pending());
        post(pvs);
        assert!(poll_once(task.as_mut()).is_pending());
    });
    let copies = Copies::default();
    run(&mut |user, ctl_rx, pvs| {
        let mut task = pin!(run_event_task(
            user,
            ctl_rx,
            copies.clone(),
            crate::server::tcp::CircuitMinor::new(CA_MINOR_VERSION)
        ));
        assert!(poll_once(task.as_mut()).is_pending());
        post(pvs);
        assert!(poll_once(task.as_mut()).is_pending());
    });
    let in_place = wire.writes().concat();
    let apart = copies.0.lock().unwrap().clone();
    assert_eq!(in_place, apart);
    // Post order, each frame whole: sub_id N..1, value N-0.5..0.5.
    let frames: Vec<&[u8]> = in_place.chunks(in_place.len() / N as usize).collect();
    assert_eq!(frames.len(), N as usize);
    for (k, f) in frames.iter().enumerate() {
        let sub = N - k as u32;
        assert_eq!(u16::from_be_bytes([f[0], f[1]]), CA_PROTO_EVENT_ADD);
        assert_eq!(u32::from_be_bytes(f[12..16].try_into().unwrap()), sub);
        let v = f64::from_be_bytes(f[16..24].try_into().unwrap());
        assert_eq!(v, (sub - 1) as f64 + 0.5);
    }
}

fn poll_once<F: Future>(fut: std::pin::Pin<&mut F>) -> Poll<F::Output> {
    fut.poll(&mut Context::from_waker(Waker::noop()))
}

/// `k` SimplePvs, each with one monitor reader on `user`, handed over as
/// `Add`s for cids `0..k`.
fn monitored_pvs(db: &PvDatabase, user: &Arc<EventUser>, k: u32) -> Vec<MonitorDelivery> {
    (0..k)
        .map(|cid| {
            let name = format!("SENDBUF:{cid}");
            block_on_sync(db.add_pv(&name, EpicsValue::Double(1.0)))
                .unwrap()
                .expect("add_pv");
            let pv = db.find_pv(&name).expect("added");
            let reader = pv
                .add_subscriber_on(
                    user,
                    cid + 1,
                    DbFieldType::Double,
                    (EventMask::VALUE | EventMask::LOG).bits(),
                    no_context(),
                )
                .expect("under the per-PV cap");
            MonitorDelivery {
                reader,
                target: ChannelTarget::SimplePv(pv),
                cid,
            }
        })
        .collect()
}

fn destroy(db: &PvDatabase, k: u32) {
    for cid in 0..k {
        block_on_sync(db.remove_simple_pv(format!("SENDBUF:{cid}")))
            .unwrap()
            .expect("registered");
    }
}

/// The `Add` arm: channels destroyed before their `Add` arrives are `k`
/// ready SERVER_DISCONN frames, one write.
#[test]
fn retirements_handed_over_together_reach_the_wire_in_one_write() {
    for k in [1u32, 3] {
        let db = PvDatabase::new();
        let user = Arc::new(EventUser::new());
        let subs = monitored_pvs(&db, &user, k);
        destroy(&db, k);
        let (ctl_tx, ctl_rx) = mpsc::unbounded_channel();
        for d in subs {
            ctl_tx.send(EventTaskControl::Add(Box::new(d))).unwrap();
        }
        let wire = Arc::new(Wire::default());
        let sink = EventSendBuf(Arc::new(Mutex::new(SendBuf::new(wire.clone()))));
        let mut task = pin!(run_event_task(
            user,
            ctl_rx,
            sink,
            crate::server::tcp::CircuitMinor::new(CA_MINOR_VERSION)
        ));
        assert!(poll_once(task.as_mut()).is_pending());
        let writes = wire.writes();
        assert_eq!(writes.len(), 1, "{k} ready frames, one write");
        assert_eq!(writes[0].len(), 16 * k as usize);
        drop(ctl_tx);
        assert!(poll_once(task.as_mut()).is_ready());
        assert_eq!(wire.writes().len(), 1, "nothing was left staged");
    }
}

/// The `Ended` arm: live monitors whose channels are destroyed together end
/// `k` streams at once, one write.
#[test]
fn streams_ended_together_reach_the_wire_in_one_write() {
    for k in [1u32, 3] {
        let db = PvDatabase::new();
        let user = Arc::new(EventUser::new());
        let subs = monitored_pvs(&db, &user, k);
        let (ctl_tx, ctl_rx) = mpsc::unbounded_channel();
        for d in subs {
            ctl_tx.send(EventTaskControl::Add(Box::new(d))).unwrap();
        }
        let wire = Arc::new(Wire::default());
        let sink = EventSendBuf(Arc::new(Mutex::new(SendBuf::new(wire.clone()))));
        let mut task = pin!(run_event_task(
            user,
            ctl_rx,
            sink,
            crate::server::tcp::CircuitMinor::new(CA_MINOR_VERSION)
        ));
        assert!(poll_once(task.as_mut()).is_pending());
        assert!(wire.writes().is_empty(), "live monitors owe no frame");
        destroy(&db, k);
        assert!(poll_once(task.as_mut()).is_pending());
        let writes = wire.writes();
        assert_eq!(writes.len(), 1, "{k} ended streams, one write");
        assert_eq!(writes[0].len(), 16 * k as usize);
        drop(ctl_tx);
        assert!(poll_once(task.as_mut()).is_ready());
    }
}

/// A socket that only counts what reaches it, so the window below measures
/// the event thread and not the test's recorder.
#[derive(Default)]
struct CountWire(AtomicUsize);

impl Write for &CountWire {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.fetch_add(bytes.len(), Ordering::Relaxed);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// `n` SimplePvs, each with one `CircuitMonitor` on `user` handed over as an
/// `Add`, so their updates take the real delivery arm.
fn circuit_monitored_pvs(
    n: u32,
    user: &Arc<EventUser>,
    ctl_tx: &mpsc::UnboundedSender<EventTaskControl>,
) -> Vec<Arc<epics_base_rs::server::pv::ProcessVariable>> {
    gated_circuit_monitored_pvs(n, user, ctl_tx)
        .into_iter()
        .map(|(pv, _)| pv)
        .collect()
}

/// [`circuit_monitored_pvs`] with each monitor, for cancelling one.
fn gated_circuit_monitored_pvs(
    n: u32,
    user: &Arc<EventUser>,
    ctl_tx: &mpsc::UnboundedSender<EventTaskControl>,
) -> Vec<(
    Arc<epics_base_rs::server::pv::ProcessVariable>,
    Arc<crate::server::tcp::CircuitMonitor>,
)> {
    use epics_base_rs::server::pv::ProcessVariable;
    use epics_base_rs::types::DBR_DOUBLE;

    let db = Arc::new(PvDatabase::new());
    (0..n)
        .map(|cid| {
            let pv = Arc::new(ProcessVariable::new(
                format!("CIRCUIT:{cid}"),
                EpicsValue::Double(0.0),
            ));
            let monitor = Arc::new(crate::server::tcp::CircuitMonitor {
                gate: crate::server::outbox::MonitorGate::open(),
                denied: AtomicBool::new(false),
                ended: AtomicBool::new(false),
                data_type: DBR_DOUBLE,
                sub_id: cid + 1,
                data_count: 0,
                stats: None,
                stage: crate::server::DeliveryStage::plain(),
                target: ChannelTarget::SimplePv(pv.clone()),
                req_hdr: CaHeader::new(CA_PROTO_EVENT_ADD),
                cid,
                db: db.clone(),
            });
            let context: epics_base_rs::server::event_queue::DeliveryContext = monitor.clone();
            let reader = pv
                .add_subscriber_on(
                    user,
                    cid + 1,
                    DbFieldType::Double,
                    EventMask::VALUE.bits(),
                    &context,
                )
                .expect("under the per-PV cap");
            ctl_tx
                .send(EventTaskControl::Add(Box::new(MonitorDelivery {
                    reader,
                    target: ChannelTarget::SimplePv(pv.clone()),
                    cid,
                })))
                .unwrap();
            (pv, monitor)
        })
        .collect()
}

/// Updates ready together leave in one write: a send per delivered frame is
/// one loopback send per monitor update, the CAS-event cost this batching
/// exists to remove.
#[test]
fn updates_ready_together_reach_the_wire_in_one_write() {
    const N: u32 = 8;
    let user = Arc::new(EventUser::new());
    let (ctl_tx, ctl_rx) = mpsc::unbounded_channel();
    let pvs = circuit_monitored_pvs(N, &user, &ctl_tx);
    let wire = Arc::new(Wire::default());
    let sink = EventSendBuf(Arc::new(Mutex::new(SendBuf::new(wire.clone()))));
    let mut task = pin!(run_event_task(
        user,
        ctl_rx,
        sink,
        crate::server::tcp::CircuitMinor::new(CA_MINOR_VERSION)
    ));
    assert!(poll_once(task.as_mut()).is_pending());
    for pv in &pvs {
        pv.set(EpicsValue::Double(1.0));
    }
    assert!(poll_once(task.as_mut()).is_pending());
    let writes = wire.writes();
    assert_eq!(writes.len(), 1, "{N} updates, one write");
    assert_eq!(writes[0].len() % N as usize, 0, "{N} whole frames");
    drop(ctl_tx);
    assert!(poll_once(task.as_mut()).is_ready());
}

/// What [`run_event_task`] asked of its sink, in order.
#[derive(Clone, Default)]
struct Calls(Arc<Mutex<Vec<&'static str>>>);

impl crate::server::tcp::EventSink for Calls {
    fn stage(&mut self, _: &[u8]) -> std::io::Result<()> {
        self.0.lock().unwrap().push("stage");
        Ok(())
    }
    fn send(&mut self) -> std::io::Result<()> {
        self.0.lock().unwrap().push("send");
        Ok(())
    }
}

/// C `read_reply` sends when `eventsRemaining` is false: an entry with more
/// behind it in its queue stays staged, the last one sends, and the send
/// before waiting finds nothing left.
#[test]
fn the_last_event_of_a_drained_queue_sends_what_is_staged() {
    let user = Arc::new(EventUser::new());
    let (ctl_tx, ctl_rx) = mpsc::unbounded_channel();
    let pvs = circuit_monitored_pvs(2, &user, &ctl_tx);
    let calls = Calls::default();
    let mut task = pin!(run_event_task(
        user,
        ctl_rx,
        calls.clone(),
        crate::server::tcp::CircuitMinor::new(CA_MINOR_VERSION)
    ));
    assert!(poll_once(task.as_mut()).is_pending());
    calls.0.lock().unwrap().clear();
    for pv in &pvs {
        pv.set(EpicsValue::Double(1.0));
    }
    assert!(poll_once(task.as_mut()).is_pending());
    assert_eq!(*calls.0.lock().unwrap(), ["stage", "stage", "send", "send"]);
    drop(ctl_tx);
    assert!(poll_once(task.as_mut()).is_ready());
}

/// An entry cancelled while in flight is the queue's last but sends nothing:
/// C `event_read` runs no callback for a log whose `user_sub` is NULL
/// (`dbEvent.c:978-993`), so `read_reply` never reaches its send.
#[test]
fn a_cancelled_last_event_sends_nothing() {
    let user = Arc::new(EventUser::new());
    let (ctl_tx, ctl_rx) = mpsc::unbounded_channel();
    let pvs = gated_circuit_monitored_pvs(2, &user, &ctl_tx);
    let calls = Calls::default();
    let mut task = pin!(run_event_task(
        user,
        ctl_rx,
        calls.clone(),
        crate::server::tcp::CircuitMinor::new(CA_MINOR_VERSION)
    ));
    assert!(poll_once(task.as_mut()).is_pending());
    calls.0.lock().unwrap().clear();
    for (pv, _) in &pvs {
        pv.set(EpicsValue::Double(1.0));
    }
    pvs[1].1.gate.revoke();
    assert!(poll_once(task.as_mut()).is_pending());
    assert_eq!(
        *calls.0.lock().unwrap(),
        ["stage", "send"],
        "the first entry stays staged until the send before waiting"
    );
    drop(ctl_tx);
    assert!(poll_once(task.as_mut()).is_ready());
}

/// A [`Calls`] whose sends fail once the peer is gone.
#[derive(Clone, Default)]
struct GoneCalls(Calls, Arc<AtomicBool>);

impl crate::server::tcp::EventSink for GoneCalls {
    fn stage(&mut self, frame: &[u8]) -> std::io::Result<()> {
        self.0.stage(frame)
    }
    fn send(&mut self) -> std::io::Result<()> {
        self.0.send()?;
        if self.1.load(Ordering::Relaxed) {
            return Err(std::io::ErrorKind::BrokenPipe.into());
        }
        Ok(())
    }
}

/// The send at the end of a drained queue is where a peer that went away
/// after the last successful send shows up first, and it ends the task there:
/// a retirement already waiting in the control queue is never staged.
#[test]
fn a_failed_send_at_a_drained_queue_ends_the_task() {
    let user = Arc::new(EventUser::new());
    let (ctl_tx, ctl_rx) = mpsc::unbounded_channel();
    let pvs = circuit_monitored_pvs(1, &user, &ctl_tx);
    let sink = GoneCalls::default();
    let mut task = pin!(run_event_task(
        user,
        ctl_rx,
        sink.clone(),
        crate::server::tcp::CircuitMinor::new(CA_MINOR_VERSION)
    ));
    assert!(poll_once(task.as_mut()).is_pending());
    assert_eq!(*sink.0.0.lock().unwrap(), ["send"], "the peer was there");
    sink.0.0.lock().unwrap().clear();
    sink.1.store(true, Ordering::Relaxed);
    pvs[0].set(EpicsValue::Double(1.0));
    // A channel destroyed before its `Add` arrives, on another circuit's
    // queue: its retirement is one staged frame, reached only if the task
    // outlives the drained-queue send.
    let db = PvDatabase::new();
    let retired = monitored_pvs(&db, &Arc::new(EventUser::new()), 1);
    destroy(&db, 1);
    for d in retired {
        ctl_tx.send(EventTaskControl::Add(Box::new(d))).unwrap();
    }
    assert!(
        poll_once(task.as_mut()).is_ready(),
        "the control sender is still held, so only the send error can end it"
    );
    assert_eq!(
        *sink.0.0.lock().unwrap(),
        ["stage", "send", "send"],
        "update staged, drained-queue send failed, exit flush"
    );
}

/// A [`Calls`] whose stages fail and whose sends do not, so only the stage
/// error can tell the task the peer is gone.
#[derive(Clone, Default)]
struct RefusedStages(Calls);

impl crate::server::tcp::EventSink for RefusedStages {
    fn stage(&mut self, frame: &[u8]) -> std::io::Result<()> {
        self.0.stage(frame)?;
        Err(std::io::ErrorKind::BrokenPipe.into())
    }
    fn send(&mut self) -> std::io::Result<()> {
        self.0.send()
    }
}

/// A failed stage ends the task at that update: the next update queued
/// behind it is never built for the gone peer.
#[test]
fn a_failed_update_stage_ends_the_task() {
    let user = Arc::new(EventUser::new());
    let (ctl_tx, ctl_rx) = mpsc::unbounded_channel();
    let pvs = circuit_monitored_pvs(2, &user, &ctl_tx);
    let sink = RefusedStages::default();
    let mut task = pin!(run_event_task(
        user,
        ctl_rx,
        sink.clone(),
        crate::server::tcp::CircuitMinor::new(CA_MINOR_VERSION)
    ));
    assert!(poll_once(task.as_mut()).is_pending());
    sink.0.0.lock().unwrap().clear();
    for pv in &pvs {
        pv.set(EpicsValue::Double(1.0));
    }
    assert!(
        poll_once(task.as_mut()).is_ready(),
        "the control sender is still held, so only the stage error can end it"
    );
    assert_eq!(
        *sink.0.0.lock().unwrap(),
        ["stage", "send"],
        "one refused stage, then the exit flush"
    );
    drop(ctl_tx);
}

/// Frames no queue drain sends, such as a retirement, go out at the send
/// before waiting, and a gone peer there ends the task too.
#[test]
fn a_failed_send_before_waiting_ends_the_task() {
    let db = PvDatabase::new();
    let user = Arc::new(EventUser::new());
    let subs = monitored_pvs(&db, &user, 1);
    destroy(&db, 1);
    let (ctl_tx, ctl_rx) = mpsc::unbounded_channel();
    for d in subs {
        ctl_tx.send(EventTaskControl::Add(Box::new(d))).unwrap();
    }
    let wire = Arc::new(Wire::default());
    wire.broken.store(true, Ordering::Relaxed);
    let sink = EventSendBuf(Arc::new(Mutex::new(SendBuf::new(wire.clone()))));
    let mut task = pin!(run_event_task(
        user,
        ctl_rx,
        sink,
        crate::server::tcp::CircuitMinor::new(CA_MINOR_VERSION)
    ));
    assert!(
        poll_once(task.as_mut()).is_ready(),
        "the control sender is still held, so only the send error can end it"
    );
    assert!(wire.writes().is_empty());
}

/// The CAS-event thread allocates nothing per event once primed: `n`
/// updates, posted outside the window, framed and sent inside it.
#[test]
fn delivering_events_through_the_real_sink_allocates_nothing() {
    const N: u32 = 8;
    let user = Arc::new(EventUser::new());
    let (ctl_tx, ctl_rx) = mpsc::unbounded_channel();
    let pvs = circuit_monitored_pvs(N, &user, &ctl_tx);
    let wire = Arc::new(CountWire::default());
    let sink = EventSendBuf(Arc::new(Mutex::new(SendBuf::new(wire.clone()))));
    let mut task = pin!(run_event_task(
        user,
        ctl_rx,
        sink,
        crate::server::tcp::CircuitMinor::new(CA_MINOR_VERSION)
    ));
    assert!(poll_once(task.as_mut()).is_pending());

    // Prime: the first delivery's one-time growth is not per event.
    for pv in &pvs {
        pv.set(EpicsValue::Double(1.0));
    }
    assert!(poll_once(task.as_mut()).is_pending());
    let primed = wire.0.load(Ordering::Relaxed);
    assert!(primed > 0, "the priming updates were delivered");

    for pv in &pvs {
        pv.set(EpicsValue::Double(2.0));
    }
    let before = super::super::delivery_alloc_tests::allocs();
    assert!(poll_once(task.as_mut()).is_pending());
    let allocs = super::super::delivery_alloc_tests::allocs() - before;
    assert_eq!(
        wire.0.load(Ordering::Relaxed) - primed,
        primed,
        "every update was framed and sent"
    );
    assert_eq!(allocs, 0, "{N} events delivered");
    drop(ctl_tx);
    assert!(poll_once(task.as_mut()).is_ready());
}
