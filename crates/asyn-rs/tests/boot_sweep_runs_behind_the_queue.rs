//! The boot sweep at the interrupt edge — C `callbackThread::run`
//! (asynPortDriver.cpp:923-937) — finds the requests queued before the edge
//! already run, and is no `queueRequest`, so a port that is down is swept too.

use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::time::Duration;

use asyn_rs::error::AsynResult;
use asyn_rs::interrupt::InterruptFilter;
use asyn_rs::param::{ParamType, ParamValue};
use asyn_rs::port::{InterfaceMask, PortDriver, PortDriverBase, PortFlags, QueuePriority};
use asyn_rs::request::RequestOp;
use asyn_rs::runtime::{RuntimeConfig, create_port_runtime_unclaimed};
use asyn_rs::user::AsynUser;
use epics_libcom_rs::runtime::interrupt_accept::InterruptGate;

/// While it reads `true`, a [`QuadPort`] write cannot finish.
type Hold = Arc<(Mutex<bool>, Condvar)>;

/// A two-address CANBLOCK port whose writes post address 0 only, as
/// drvQuadEM's `writeInt32` does (drvQuadEM.cpp:517). Each write is a slow
/// device round trip, announced when it starts, that waits out `hold`.
struct QuadPort {
    base: PortDriverBase,
    started: mpsc::Sender<()>,
    hold: Hold,
}

impl PortDriver for QuadPort {
    fn base(&self) -> &PortDriverBase {
        &self.base
    }
    fn base_mut(&mut self) -> &mut PortDriverBase {
        &mut self.base
    }
    fn write_int32(&mut self, user: &mut AsynUser, value: i32) -> AsynResult<()> {
        let _ = self.started.send(());
        std::thread::sleep(Duration::from_millis(100));
        let (held, released) = &*self.hold;
        drop(released.wait_while(held.lock().unwrap(), |held| *held));
        let _ = self.base.set_int32_param(user.reason, user.addr, value);
        self.base.call_param_callbacks(0)
    }
}

/// A [`QuadPort`] named `name`, set up by `init`, held by `hold`, registered
/// on `gate`; with the `VAL` param, the Int32 values posted at address 1, and
/// what started writes announce. The last item keeps the port alive.
fn quad_port(
    name: &str,
    gate: &Arc<InterruptGate>,
    hold: Hold,
    init: impl FnOnce(&mut PortDriverBase, usize),
) -> (
    asyn_rs::port_handle::PortHandle,
    usize,
    mpsc::Receiver<i32>,
    mpsc::Receiver<()>,
    impl Sized,
) {
    let mut base = PortDriverBase::new(
        name,
        2,
        PortFlags {
            multi_device: true,
            can_block: true,
            ..PortFlags::default()
        },
        InterfaceMask::UNMIGRATED_ALL,
        InterfaceMask::UNMIGRATED_INTERRUPTS,
    );
    let val = base.create_param("VAL", ParamType::Int32).unwrap();
    init(&mut base, val);
    let (started_tx, started) = mpsc::channel();
    let (rt, jh) = create_port_runtime_unclaimed(
        QuadPort {
            base,
            started: started_tx,
            hold,
        },
        RuntimeConfig::default(),
    )
    .unwrap();
    let port = rt.port_handle().clone();
    let (posted_tx, posted) = mpsc::channel();
    let sub = port
        .interrupts()
        .register_sync_callback(
            InterruptFilter {
                reason: Some(val),
                addr: Some(1),
                ..Default::default()
            },
            move |iv| {
                if let ParamValue::Int32(v) = iv.value {
                    let _ = posted_tx.send(v);
                }
            },
        )
        .unwrap();
    asyn_rs::registry::register_port(name, port.clone(), gate).unwrap();
    (port, val, posted, started, (rt, jh, sub))
}

/// Boundary — five writes at address 1 queued before the edge, the first
/// still running: the sweep runs after the last, so its one address-1 post
/// carries the last value written.
#[test]
fn the_sweep_posts_the_last_write_queued_before_the_edge() {
    let gate = InterruptGate::new();
    let (port, val, posted, started, _port) =
        quad_port("SWEEP_BEHIND", &gate, Arc::default(), |_, _| {});
    let _writes: Vec<_> = (1..=5)
        .map(|value| {
            let user = AsynUser::new(val)
                .with_addr(1)
                .with_priority(QueuePriority::Low);
            port.try_submit(RequestOp::Int32Write { value }, user)
                .unwrap()
        })
        .collect();
    started.recv_timeout(Duration::from_secs(5)).unwrap();
    gate.set(true);
    assert_eq!(posted.recv_timeout(Duration::from_secs(5)), Ok(5));
}

/// Boundary — a port that is down and will not reconnect: the sweep still
/// posts the value seeded before the edge.
#[test]
fn a_disconnected_port_is_still_swept() {
    let gate = InterruptGate::new();
    let (_, _, posted, _, _port) = quad_port("SWEEP_DOWN", &gate, Arc::default(), |base, val| {
        base.init_auto_connect(false);
        base.init_connected(false);
        base.set_int32_param(val, 1, 5).unwrap();
    });
    gate.set(true);
    assert_eq!(posted.recv_timeout(Duration::from_secs(5)), Ok(5));
}

// RTEMS-EXEC-MODEL-ALLOW(1): checked, not waived — it ran and passed on the
// exec backend (`EPICS_RS_BUILD_EXEC_BACKEND=thread cargo nextest run -p
// asyn-rs --test boot_sweep_runs_behind_the_queue`, 3/3).

/// Boundary — a request queued before the edge that does not return, as a
/// driver call with no timeout on a silent device: the edge's job, which
/// iocRun's `dbCaRun` step waits for, settles while it runs, the port owes
/// the sweep meanwhile, and the sweep still posts once it returns.
#[tokio::test]
async fn the_edge_settles_while_a_request_queued_before_it_runs() {
    let gate = InterruptGate::new();
    let hold: Hold = Arc::default();
    let (port, val, posted, started, _port) =
        quad_port("SWEEP_HELD", &gate, hold.clone(), |_, _| {});
    *hold.0.lock().unwrap() = true;
    let user = AsynUser::new(val).with_addr(1);
    let _write = port
        .try_submit(RequestOp::Int32Write { value: 7 }, user)
        .unwrap();
    started.recv_timeout(Duration::from_secs(5)).unwrap();
    gate.set(true);
    tokio::time::timeout(Duration::from_secs(2), gate.jobs_settled())
        .await
        .expect("the edge's job waited for the request queued before it");
    assert_eq!(posted.try_recv(), Err(mpsc::TryRecvError::Empty));
    let sweeps = port.interrupts().edge_sweeps();
    assert!(!sweeps.is_clear(), "the sweep is owed");
    *hold.0.lock().unwrap() = false;
    hold.1.notify_all();
    tokio::time::timeout(Duration::from_secs(5), sweeps.cleared())
        .await
        .expect("the sweep ran");
    assert_eq!(
        posted.try_recv(),
        Ok(7),
        "posted before the port stopped owing it"
    );
}
