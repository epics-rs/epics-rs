use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use asyn_rs::interfaces::motor::AsynMotor;
use epics_base_rs::server::device_support::DeviceSupport;
use epics_base_rs::server::record::Record;
use epics_base_rs::types::EpicsValue;
use tokio::sync::mpsc;

use motor_rs::builder::MotorBuilder;
use motor_rs::flags::*;
use motor_rs::poll_loop::PollCommand;
use motor_rs::sim_motor::SimMotor;

mod passes;

/// The status `motor` writes polled from the default.
fn polled(
    motor: &mut (impl AsynMotor + ?Sized),
    user: &asyn_rs::user::AsynUser,
) -> asyn_rs::interfaces::motor::MotorStatus {
    let mut status = asyn_rs::interfaces::motor::MotorStatus::default();
    motor.poll(user, &mut status).unwrap();
    status
}

/// Eventual assertion — polls condition with timeout.
async fn wait_until(timeout: Duration, mut f: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if f() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    false
}

fn make_builder(motor: Arc<Mutex<dyn AsynMotor>>) -> MotorBuilder {
    MotorBuilder::new(motor)
        .poll_interval(Duration::from_millis(5))
        .configure_record(|rec| {
            rec.conv.mres = 0.001;
            rec.limits.dhlm = 100.0;
            rec.limits.dllm = -100.0;
            rec.limits.hlm = 100.0;
            rec.limits.llm = -100.0;
            rec.limits.lvio = 0;
            rec.vel.velo = 100000.0; // very fast for tests
            rec.vel.accl = 0.5;
            rec.vel.bvel = 100000.0;
            rec.vel.bacc = 0.5;
            rec.vel.hvel = 100000.0;
            rec.vel.jvel = 5.0;
            rec.vel.jar = 1.0;
            rec.stat.msta = MstaFlags::DONE;
        })
}

/// A built record is past init_record, as an IOC's is: a put on it is a
/// runtime put, whose special() HLM sets DHLM (motorRecord.cc:2796-2797).
#[test]
fn a_put_on_a_built_record_runs_special() {
    let motor: Arc<Mutex<dyn AsynMotor>> = Arc::new(Mutex::new(SimMotor::new()));
    let mut setup = make_builder(motor).build();
    setup
        .record
        .put_field("HLM", EpicsValue::Double(50.0))
        .unwrap();
    assert_eq!(setup.record.limits.dhlm, 50.0);
}

#[tokio::test]
async fn test_full_move_via_mailbox() {
    let motor: Arc<Mutex<dyn AsynMotor>> = Arc::new(Mutex::new(SimMotor::new()));
    let mut setup = make_builder(motor).build();

    // Init device support
    setup.device_support.init(&mut setup.record).unwrap();

    // Spawn poll loop
    let poll_handle = tokio::spawn(setup.poll_loop.start());

    // Consume startup event
    setup.record.process().unwrap();
    setup.device_support.write(&mut setup.record).unwrap();

    // Write VAL to start move
    setup
        .record
        .put_field("VAL", EpicsValue::Double(10.0))
        .unwrap();
    setup.record.process().unwrap();
    setup.device_support.write(&mut setup.record).unwrap();

    assert!(setup.record.stat.dmov == 0);

    // Wait for DMOV=true with polling
    let record_ref = &mut setup.record;
    let ds_ref = &mut setup.device_support;
    let reached = wait_until(Duration::from_secs(2), || {
        // Process record to pick up device updates
        record_ref.process().unwrap();
        ds_ref.write(record_ref).unwrap();
        record_ref.stat.dmov != 0
    })
    .await;

    assert!(reached, "DMOV should become true after move completes");
    assert!((setup.record.pos.rbv - 10.0).abs() < 0.1);

    // Shutdown
    let _ = setup.poll_cmd_tx.send(PollCommand::Shutdown).await;
    let _ = poll_handle.await;
}

#[tokio::test]
async fn test_stop_during_move() {
    let motor: Arc<Mutex<dyn AsynMotor>> = Arc::new(Mutex::new(SimMotor::new()));
    let mut setup = make_builder(motor).build();

    setup.device_support.init(&mut setup.record).unwrap();
    let poll_handle = tokio::spawn(setup.poll_loop.start());

    // Consume startup event
    setup.record.process().unwrap();
    setup.device_support.write(&mut setup.record).unwrap();

    // Start a slow move (velocity=1, target=50 → 50s)
    setup.record.vel.velo = 1.0;
    setup
        .record
        .put_field("VAL", EpicsValue::Double(50.0))
        .unwrap();
    setup.record.process().unwrap();
    setup.device_support.write(&mut setup.record).unwrap();

    assert!(setup.record.stat.dmov == 0);

    // Let it move a bit
    tokio::time::sleep(Duration::from_millis(50)).await;
    setup.record.process().unwrap();
    setup.device_support.write(&mut setup.record).unwrap();

    // Issue STOP
    setup
        .record
        .put_field("STOP", EpicsValue::Short(1))
        .unwrap();
    setup.record.process().unwrap();
    setup.device_support.write(&mut setup.record).unwrap();

    // Wait for DMOV
    let record_ref = &mut setup.record;
    let ds_ref = &mut setup.device_support;
    let reached = wait_until(Duration::from_secs(2), || {
        record_ref.process().unwrap();
        ds_ref.write(record_ref).unwrap();
        record_ref.stat.dmov != 0
    })
    .await;

    assert!(reached, "DMOV should become true after stop");
    assert!(
        setup.record.pos.rbv < 50.0,
        "motor should not have reached target"
    );

    let _ = setup.poll_cmd_tx.send(PollCommand::Shutdown).await;
    let _ = poll_handle.await;
}

#[tokio::test]
async fn test_delay_via_poll_loop() {
    let motor: Arc<Mutex<dyn AsynMotor>> = Arc::new(Mutex::new(SimMotor::new()));
    let mut setup = make_builder(motor)
        .configure_record(|rec| {
            rec.conv.mres = 0.001;
            rec.limits.dhlm = 100.0;
            rec.limits.dllm = -100.0;
            rec.limits.hlm = 100.0;
            rec.limits.llm = -100.0;
            rec.limits.lvio = 0;
            rec.vel.velo = 100000.0; // very fast
            rec.vel.accl = 0.5;
            rec.vel.bvel = 5.0;
            rec.vel.bacc = 0.5;
            rec.stat.msta = MstaFlags::DONE;
            rec.timing.dly = 0.05; // 50ms delay
        })
        .build();

    setup.device_support.init(&mut setup.record).unwrap();
    let poll_handle = tokio::spawn(setup.poll_loop.start());

    // Consume startup event
    setup.record.process().unwrap();
    setup.device_support.write(&mut setup.record).unwrap();

    // Start move
    setup
        .record
        .put_field("VAL", EpicsValue::Double(5.0))
        .unwrap();
    setup.record.process().unwrap();
    setup.device_support.write(&mut setup.record).unwrap();

    // Wait for DMOV=true (should take >50ms due to DLY)
    let start = Instant::now();
    let record_ref = &mut setup.record;
    let ds_ref = &mut setup.device_support;
    let reached = wait_until(Duration::from_secs(2), || {
        record_ref.process().unwrap();
        ds_ref.write(record_ref).unwrap();
        record_ref.stat.dmov != 0
    })
    .await;

    assert!(reached, "DMOV should become true after delay");
    let elapsed = start.elapsed();
    assert!(
        elapsed >= Duration::from_millis(40),
        "expected at least ~50ms delay, got {:?}",
        elapsed
    );

    let _ = setup.poll_cmd_tx.send(PollCommand::Shutdown).await;
    let _ = poll_handle.await;
}

#[tokio::test]
async fn test_backlash_via_mailbox() {
    // The backlash leg ends at -11 EGU, past the sim's default -10000-step
    // switch; an LS stop would end the move there (C 1367-1380).
    let motor: Arc<Mutex<dyn AsynMotor>> = Arc::new(Mutex::new(
        SimMotor::new().with_limits(-100_000.0, 100_000.0),
    ));
    let mut setup = make_builder(motor)
        .configure_record(|rec| {
            rec.conv.mres = 0.001;
            rec.limits.dhlm = 100.0;
            rec.limits.dllm = -100.0;
            rec.limits.hlm = 100.0;
            rec.limits.llm = -100.0;
            rec.limits.lvio = 0;
            rec.vel.velo = 100000.0;
            rec.vel.accl = 0.5;
            rec.vel.bvel = 100000.0;
            rec.vel.bacc = 0.5;
            rec.stat.msta = MstaFlags::DONE;
            rec.retry.bdst = 1.0; // positive backlash
        })
        .build();

    setup.device_support.init(&mut setup.record).unwrap();
    let poll_handle = tokio::spawn(setup.poll_loop.start());

    // Consume startup event
    setup.record.process().unwrap();
    setup.device_support.write(&mut setup.record).unwrap();

    // Move in negative direction to trigger backlash
    setup
        .record
        .put_field("VAL", EpicsValue::Double(-10.0))
        .unwrap();
    setup.record.process().unwrap();
    setup.device_support.write(&mut setup.record).unwrap();

    assert!(setup.record.stat.dmov == 0);

    // Wait for DMOV
    let record_ref = &mut setup.record;
    let ds_ref = &mut setup.device_support;
    let reached = wait_until(Duration::from_secs(2), || {
        record_ref.process().unwrap();
        ds_ref.write(record_ref).unwrap();
        record_ref.stat.dmov != 0
    })
    .await;

    assert!(reached, "DMOV should become true after backlash");
    assert!(
        (setup.record.pos.rbv - (-10.0)).abs() < 0.1,
        "final position should be near -10.0, got {}",
        setup.record.pos.rbv
    );

    let _ = setup.poll_cmd_tx.send(PollCommand::Shutdown).await;
    let _ = poll_handle.await;
}

#[tokio::test]
async fn test_poll_loop_lifecycle() {
    let motor: Arc<Mutex<dyn AsynMotor>> = Arc::new(Mutex::new(SimMotor::new()));
    let (poll_cmd_tx, poll_cmd_rx) = mpsc::channel(16);
    let device_state = motor_rs::device_state::new_shared_state();
    // Past finishDevSup: each post processes the record.
    let trace = Arc::default();
    let reads = passes::bind(&device_state, &trace);
    let (io_intr_tx, _io_intr_rx) = mpsc::channel::<()>(16);

    let poll_loop = motor_rs::poll_loop::MotorPollLoop::new(
        motor_rs::poll_loop::PollerInbox {
            poll: poll_cmd_rx,
            commands: mpsc::unbounded_channel().1,
        },
        motor,
        {
            let subscribers = motor_rs::device_state::AxisSubscribers::new();
            subscribers.attach(device_state.clone(), io_intr_tx, trace);
            subscribers
        },
        Duration::from_millis(5),
        Duration::from_millis(5),
        0,
    );

    let poll_handle = tokio::spawn(poll_loop.start());

    // Start polling
    poll_cmd_tx.send(PollCommand::ForcePoll).await.unwrap();

    // Wait for the poll loop's first process of the record.
    let read = reads.wait(Duration::from_millis(500)).await;
    assert!(!read.is_empty(), "the poll loop should process the record");
    assert!(read[0].is_some(), "status should be populated");

    // Shutdown
    poll_cmd_tx.send(PollCommand::Shutdown).await.unwrap();
    let result = tokio::time::timeout(Duration::from_secs(1), poll_handle).await;
    assert!(result.is_ok(), "poll loop should terminate after Shutdown");
}

/// R61: a zero idle poll interval is event-only, never a busy-spin.
/// C `asynMotorController::asynMotorPoller` (asynMotorController.cpp:633-634)
/// treats `idlePollPeriod_ == 0` as "block on the event, no timed poll". The
/// Rust loop guards the `sleep(interval)` select arm on `!interval.is_zero()`,
/// so with a settled (done, not-moving) motor ForcePoll triggers exactly
/// one poll and the loop then blocks on the command channel. Without the guard
/// `sleep(Duration::ZERO)` would fire immediately on every iteration, spinning
/// the CPU and flooding io_intr.
#[tokio::test]
async fn test_zero_idle_interval_is_event_only_not_busy_spin() {
    let motor: Arc<Mutex<dyn AsynMotor>> = Arc::new(Mutex::new(SimMotor::new()));
    let (poll_cmd_tx, poll_cmd_rx) = mpsc::channel(16);
    let device_state = motor_rs::device_state::new_shared_state();
    let (io_intr_tx, mut io_intr_rx) = mpsc::channel::<()>(16);

    // idle interval 0 == C idlePollPeriod_ == 0 (event-only); moving 5ms.
    let poll_loop = motor_rs::poll_loop::MotorPollLoop::new(
        motor_rs::poll_loop::PollerInbox {
            poll: poll_cmd_rx,
            commands: mpsc::unbounded_channel().1,
        },
        motor,
        {
            let subscribers = motor_rs::device_state::AxisSubscribers::new();
            subscribers.attach(device_state.clone(), io_intr_tx, Default::default());
            subscribers
        },
        Duration::from_millis(5),
        Duration::ZERO,
        0,
    );
    let poll_handle = tokio::spawn(poll_loop.start());

    // Continuously drain io_intr so a (would-be) busy-spin is NOT throttled by
    // channel backpressure — without draining, a spin stalls on the full
    // channel and masks itself.
    tokio::spawn(async move { while io_intr_rx.recv().await.is_some() {} });

    poll_cmd_tx.send(PollCommand::ForcePoll).await.unwrap();

    // seq starts at 1 (init), +1 for the startup pass's post of its changed
    // status to the record already bound (asynMotorController.cpp:630,
    // asynMotorAxis.cpp:318-320), +1 for the single ForcePoll poll → 3.
    // A busy-spin would drive it into the hundreds/thousands.
    let seq = || {
        device_state
            .lock()
            .unwrap()
            .latest_status
            .as_ref()
            .map(|s| s.seq)
            .unwrap_or(0)
    };
    assert!(wait_until(Duration::from_secs(10), || seq() >= 3).await);
    // Give any erroneous busy-spin a 100ms window to accumulate polls.
    tokio::time::sleep(Duration::from_millis(100)).await;
    let seq = seq();
    assert_eq!(
        seq, 3,
        "zero idle interval must poll once on ForcePoll then block (event-only), got seq {seq}"
    );

    let _ = poll_cmd_tx.send(PollCommand::Shutdown).await;
    let _ = poll_handle.await;
}

/// Idle change-detection: a stationary axis polled at a NON-zero idle interval
/// must not re-post on every poll. C `asynMotorAxis::callParamCallbacks`
/// (asynMotorAxis.cpp:316-322) fires the status callback — which drives the
/// record's process() — only when `statusChanged_` is set; on an unchanging
/// idle poll C posts nothing. A *forced* poll (ForcePoll, the analogue of
/// C `motorUpdateStatus_` forcing `statusChanged_=1`) still posts even when the
/// status is identical, so STUP/GET_INFO acks and readback refreshes are never
/// stranded.
#[tokio::test]
async fn test_idle_poll_change_detection_suppresses_unchanged_reposts() {
    let motor: Arc<Mutex<dyn AsynMotor>> = Arc::new(Mutex::new(SimMotor::new()));
    let (poll_cmd_tx, poll_cmd_rx) = mpsc::channel(16);
    let device_state = motor_rs::device_state::new_shared_state();
    let (io_intr_tx, mut io_intr_rx) = mpsc::channel::<()>(64);

    // Non-zero idle interval so the timed arm fires repeatedly; moving 5ms.
    let poll_loop = motor_rs::poll_loop::MotorPollLoop::new(
        motor_rs::poll_loop::PollerInbox {
            poll: poll_cmd_rx,
            commands: mpsc::unbounded_channel().1,
        },
        motor,
        {
            let subscribers = motor_rs::device_state::AxisSubscribers::new();
            subscribers.attach(device_state.clone(), io_intr_tx, Default::default());
            subscribers
        },
        Duration::from_millis(5),
        Duration::from_millis(5),
        0,
    );
    let poll_handle = tokio::spawn(poll_loop.start());

    // Drain io_intr so a (would-be) re-post spree is not throttled by the
    // channel and is observable through the seq.
    tokio::spawn(async move { while io_intr_rx.recv().await.is_some() {} });

    let seq = |ds: &motor_rs::device_state::SharedDeviceState| {
        ds.lock()
            .unwrap()
            .latest_status
            .as_ref()
            .map(|s| s.seq)
            .unwrap_or(0)
    };

    // The startup pass posts its changed status to the bound record
    // (asynMotorController.cpp:630, asynMotorAxis.cpp:318-320; seq 1→2);
    // ForcePoll forces one more → 3.
    poll_cmd_tx.send(PollCommand::ForcePoll).await.unwrap();
    assert!(wait_until(Duration::from_secs(10), || seq(&device_state) >= 3).await);
    tokio::time::sleep(Duration::from_millis(80)).await;
    assert_eq!(
        seq(&device_state),
        3,
        "a stationary axis must not re-post on every idle poll (change-gated)"
    );

    // A second ForcePoll is a FORCED poll → re-posts even though the status
    // is byte-identical → seq 3→4.
    poll_cmd_tx.send(PollCommand::ForcePoll).await.unwrap();
    assert!(wait_until(Duration::from_secs(10), || seq(&device_state) >= 4).await);
    tokio::time::sleep(Duration::from_millis(80)).await;
    assert_eq!(
        seq(&device_state),
        4,
        "a forced poll must post even when status is unchanged (STUP/GET_INFO ack)"
    );

    let _ = poll_cmd_tx.send(PollCommand::Shutdown).await;
    let _ = poll_handle.await;
}

/// A JOGF held through a positional move strands, as C does: special() arms
/// MIP_JOG_REQ only at mip==MIP_DONE (motorRecord.cc:3045), so no pass the
/// poller's callbacks run after the close-enough completion starts the jog,
/// and a fresh press on the idle record does. The record is processed only
/// by those callbacks and the puts, the production wiring.
#[tokio::test(flavor = "multi_thread")]
async fn test_jog_held_through_a_move_strands_until_pressed_again() {
    use epics_base_rs::server::database::PvDatabase;
    use epics_base_rs::server::record::ScanType;
    let motor: Arc<Mutex<dyn AsynMotor>> = Arc::new(Mutex::new(SimMotor::new()));
    let mut setup = make_builder(motor).build();

    let db = Arc::new(PvDatabase::new());
    let mut dev = setup.device_support;
    dev.set_record_info("M", ScanType::Passive);
    dev.bind_database(db.async_handle());
    dev.bind_dev_sup_gate(db.scan_control().dev_sup_gate());
    dev.init(&mut setup.record).unwrap();
    db.add_record("M", Box::new(setup.record)).await.unwrap();
    {
        let rec = db.get_record("M").unwrap();
        let mut inst = rec.write();
        inst.common.dtyp = "simMotor".into();
        inst.device = Some(Box::new(dev));
    }
    let poll_handle = tokio::spawn(setup.poll_loop.start());
    // Startup pass, then finishDevSup.
    db.process_record_readback("M", &mut epics_base_rs::server::database::ProcStack::new())
        .await
        .unwrap();
    db.scan_control().finish_dev_sup();
    let motor_record = |f: &dyn Fn(&motor_rs::MotorRecord) -> bool| {
        let rec = db.get_record("M").unwrap();
        let mut inst = rec.write();
        let record = inst.record.as_any_mut().unwrap();
        f(record.downcast_ref::<motor_rs::MotorRecord>().unwrap())
    };
    let put = |field: &'static str, value: EpicsValue| {
        let db = db.clone();
        async move {
            db.put_record_field_from_ca_no_notify("M", field, value)
                .await
                .unwrap()
        }
    };

    // Moderate speed (≈40ms over several 5ms polls), so the poller posts
    // genuine moving-phase callbacks before completion; lands within RDBD
    // of the target (close-enough on arrival).
    put("VELO", EpicsValue::Double(50.0)).await;
    put("RDBD", EpicsValue::Double(0.5)).await;
    put("VAL", EpicsValue::Double(2.0)).await;
    // Operator holds JOGF DURING the move (mip == MOVE, not DONE): MIP_JOG_REQ
    // is never armed — exactly the case C strands.
    put("JOGF", EpicsValue::Short(1)).await;

    // Phase 1: the move completes (finalize runs; the jog is NOT fired in
    // the completion pass).
    let completed = wait_until(Duration::from_secs(2), || {
        motor_record(&|r| r.stat.dmov != 0 && r.stat.phase == MotionPhase::Idle)
    })
    .await;
    assert!(completed, "the positional move must complete close-enough");

    // Phase 2: every pass the poller's callbacks still run leaves the axis
    // idle.
    let jogged = wait_until(Duration::from_millis(300), || {
        motor_record(&|r| r.stat.phase == MotionPhase::Jog)
    })
    .await;
    assert!(!jogged, "a callback pass started the stranded jog");
    assert!(
        motor_record(&|r| r.ctrl.jogf != 0),
        "the button stays latched"
    );

    // Phase 3: a fresh press on the idle record arms and starts the jog.
    put("JOGF", EpicsValue::Short(1)).await;
    assert!(motor_record(&|r| r.stat.phase == MotionPhase::Jog));

    let _ = setup.poll_cmd_tx.send(PollCommand::Shutdown).await;
    let _ = poll_handle.await;
}

fn read_motor_pos(motor: &Arc<Mutex<dyn AsynMotor>>) -> f64 {
    let mut m = motor.lock().unwrap();
    polled(&mut *m, &asyn_rs::user::AsynUser::new(0)).position
}

/// R59 regression: device-support `init()` must NOT reseed the controller.
/// The RSTM/loadpos(#231)/MRES(#196) restore decision is owned by
/// `initial_readback` (C `devMotorAsyn.c::init_controller`), which fires on
/// the Startup process and tests the controller's *actual current position*.
/// A controller that kept its true absolute position across an IOC restart
/// must not be clobbered by a stale autosaved DVAL: with the controller far
/// from zero, RSTM=NearZero's `dval_non_zero_pos_near_zero` gate is false, so
/// no reseed occurs and the record adopts the controller readback. (An
/// earlier Rust `init()` reseeded unconditionally on any pass0 restore,
/// before the first poll — the exact data loss RSTM prevents. C does NOT
/// reseed here: a saved DVAL that the live controller contradicts, with the
/// controller nowhere near zero, fails `initPos`.)
#[tokio::test]
async fn init_does_not_clobber_live_controller_position() {
    // The controller (SimMotor) kept its true position 5.0 across the restart.
    let motor: Arc<Mutex<dyn AsynMotor>> = Arc::new(Mutex::new(SimMotor::new().with_position(5.0)));
    let mut setup = make_builder(motor.clone()).build();
    // The poller's startup pass ran before init.
    setup.poll_loop.run_startup_pass();
    setup.record.conv.mres = 1.0; // dial == raw for a clean assertion
    setup.record.conv.rstm = RestoreMode::NearZero;
    setup.record.retry.rdbd = 0.05;

    // Autosave restored a STALE DVAL (2.0) that disagrees with the hardware.
    setup
        .record
        .put_field("DVAL", EpicsValue::Double(2.0))
        .unwrap();
    assert!(setup.record.was_position_restored());

    // init() polls the controller (true 5.0) and reseeds nothing.
    setup.device_support.init(&mut setup.record).unwrap();
    assert!(
        (read_motor_pos(&motor) - 5.0).abs() < 1e-9,
        "init() must not reseed — controller stays at its true 5.0"
    );

    // The Startup process applies the RSTM gate: the controller is far from
    // zero, so NearZero does not restore — no SetPosition reaches the driver.
    setup.record.process().unwrap();
    setup.device_support.write(&mut setup.record).unwrap();
    setup.poll_loop.run_queued_commands();
    assert!(
        (read_motor_pos(&motor) - 5.0).abs() < 1e-9,
        "RSTM=NearZero with a live controller far from zero must not reseed"
    );
    assert!(
        (setup.record.pos.dval - 5.0).abs() < 1e-9,
        "record adopts the controller readback (5.0), discarding the stale 2.0"
    );
}

/// R59 companion (positive path): when the controller lost its position
/// across the restart (reads near zero) and a meaningful DVAL was restored,
/// the RSTM=NearZero gate DOES reseed — and the SetPosition now reaches the
/// driver through the Startup process → pending_actions → write path, not the
/// deleted `init()` reseed.
#[tokio::test]
async fn startup_reseeds_controller_that_lost_position() {
    // Controller powered up at 0.0 (lost its absolute position).
    let motor: Arc<Mutex<dyn AsynMotor>> = Arc::new(Mutex::new(SimMotor::new().with_position(0.0)));
    let mut setup = make_builder(motor.clone()).build();
    setup.record.conv.mres = 1.0;
    setup.record.conv.rstm = RestoreMode::NearZero;
    setup.record.retry.rdbd = 0.05;

    // Autosave restored a meaningful DVAL the controller no longer holds.
    setup
        .record
        .put_field("DVAL", EpicsValue::Double(5.0))
        .unwrap();

    setup.device_support.init(&mut setup.record).unwrap();
    assert!(
        read_motor_pos(&motor).abs() < 1e-9,
        "init() does not reseed; controller still at 0.0"
    );

    // The Startup process restores: controller near zero + meaningful DVAL.
    setup.record.process().unwrap();
    setup.device_support.write(&mut setup.record).unwrap();
    setup.poll_loop.run_queued_commands();
    assert!(
        (read_motor_pos(&motor) - 5.0).abs() < 1e-9,
        "RSTM=NearZero reseeds the lost controller to the autosaved 5.0"
    );
    assert!(
        (setup.record.pos.dval - 5.0).abs() < 1e-9,
        "record keeps the autosaved DVAL after the restore"
    );
}

/// R59 companion: when NO position was restored during pass0, neither
/// `init()` nor the Startup process reseeds — the controller keeps its
/// hardware position and the record adopts the readback.
#[tokio::test]
async fn init_does_not_reseed_controller_when_nothing_restored() {
    let motor: Arc<Mutex<dyn AsynMotor>> = Arc::new(Mutex::new(SimMotor::new().with_position(5.0)));
    let mut setup = make_builder(motor.clone()).build();
    setup.record.conv.mres = 1.0;

    // No put_field("DVAL", ...) — nothing was restored.
    assert!(!setup.record.was_position_restored());

    setup.device_support.init(&mut setup.record).unwrap();
    setup.record.process().unwrap();
    setup.device_support.write(&mut setup.record).unwrap();
    setup.poll_loop.run_queued_commands();

    assert!(
        (read_motor_pos(&motor) - 5.0).abs() < 1e-9,
        "controller untouched when nothing was restored"
    );
}

/// C init_record 717-718 ("Reset limits in case database values are
/// invalid"): both dial limits are forwarded to the driver at init,
/// after the initial readback.
#[tokio::test]
async fn init_forwards_dial_limits_to_driver() {
    let sim = Arc::new(Mutex::new(SimMotor::new()));
    let motor: Arc<Mutex<dyn AsynMotor>> = sim.clone();
    let mut setup = make_builder(motor).build();

    setup.device_support.init(&mut setup.record).unwrap();
    setup.poll_loop.run_queued_commands();

    let m = sim.lock().unwrap();
    assert_eq!(m.forwarded_high_limit, Some(100_000.0));
    assert_eq!(m.forwarded_low_limit, Some(-100_000.0));
}

/// C special set_dial_highlimit (motorRecord.cc:4236-4277): a runtime
/// DHLM put sends the new limit; devMotorAsyn lands it on the
/// motorHighLimit parameter.
#[tokio::test]
async fn limit_put_forward_reaches_driver() {
    let sim = Arc::new(Mutex::new(SimMotor::new()));
    let motor: Arc<Mutex<dyn AsynMotor>> = sim.clone();
    let mut setup = make_builder(motor).build();

    setup.device_support.init(&mut setup.record).unwrap();
    // The builder harness skips the record init passes; the live IOC
    // sets this in init_record(pass1).
    setup.record.internal.init_invariants_synced = true;
    setup.record.process().unwrap();
    setup.device_support.write(&mut setup.record).unwrap();
    setup.poll_loop.run_queued_commands();

    setup
        .record
        .put_field("DHLM", EpicsValue::Double(50.0))
        .unwrap();
    setup.record.process().unwrap();
    setup.device_support.write(&mut setup.record).unwrap();
    setup.poll_loop.run_queued_commands();

    assert_eq!(sim.lock().unwrap().forwarded_high_limit, Some(50_000.0));
}

/// C special pidcof (motorRecord.cc:3003-3026): a PCOF put under
/// GAIN_SUPPORT sends SET_PGAIN with the clamped value; devMotorAsyn
/// lands it on motorPGain.
#[tokio::test]
async fn pid_gain_put_forward_reaches_driver() {
    let sim = Arc::new(Mutex::new(SimMotor::new()));
    let motor: Arc<Mutex<dyn AsynMotor>> = sim.clone();
    let mut setup = make_builder(motor).build();

    setup.device_support.init(&mut setup.record).unwrap();
    setup.record.process().unwrap();
    setup.device_support.write(&mut setup.record).unwrap();
    setup.poll_loop.run_queued_commands();

    // SimMotor's status does not report gain support; model a
    // gain-capable controller the way C reads it — from MSTA.
    setup.record.stat.msta.insert(MstaFlags::GAIN_SUPPORT);
    setup
        .record
        .put_field("PCOF", EpicsValue::Double(1.5))
        .unwrap();
    setup.record.process().unwrap();
    setup.device_support.write(&mut setup.record).unwrap();
    setup.poll_loop.run_queued_commands();

    assert_eq!(
        sim.lock().unwrap().pid_gains,
        vec![(PidGainKind::Proportional, 1.0)],
        "clamped before emission (C 3005-3017)"
    );
}

/// R6-50: a failed poll must still post a status carrying COMM_ERR.
///
/// C drivers signal a poll failure by setting `motorStatusProblem_` and
/// `motorStatusCommsError_` and calling `callParamCallbacks()` anyway before
/// returning the error (`smarActMCSMotorDriver.cpp:503-507`,
/// `XPSAxis.cpp:756`); `asynMotorController` discards the returned status on
/// both poll paths (`asynMotorController.cpp:219-221` forced, `:658`
/// background). The failure reaches the record only as MSTA bit 12, which
/// `alarm_sub` turns into a COMM/INVALID alarm (`motorRecord.cc:3392-3398`).
///
/// The Rust poll loop used to `return` on `Err`, skipping the sequence bump,
/// the status write and the io_intr pulse — so no COMM_ERR, no alarm, no
/// record process, and a `STUP=BUSY` refresh latched at BUSY forever.
#[tokio::test]
async fn failed_poll_posts_comms_error_status() {
    use asyn_rs::error::{AsynError, AsynStatus};
    use asyn_rs::interfaces::motor::MotorStatus;
    use asyn_rs::user::AsynUser;
    use std::sync::atomic::{AtomicBool, Ordering};

    /// A motor whose transport can be cut at will.
    struct FlakyMotor {
        offline: Arc<AtomicBool>,
        position: f64,
    }
    impl AsynMotor for FlakyMotor {
        fn poll_failure(&self) -> asyn_rs::interfaces::motor::PollFailure {
            asyn_rs::interfaces::motor::PollFailure::PROBLEM_AND_COMMS_ERROR
        }
        fn poll(&mut self, _user: &AsynUser, status: &mut MotorStatus) -> Result<(), AsynError> {
            if self.offline.load(Ordering::SeqCst) {
                return Err(AsynError::Status {
                    status: AsynStatus::Timeout,
                    message: "controller not responding".into(),
                });
            }
            *status = MotorStatus {
                position: self.position,
                done: true,
                ..MotorStatus::default()
            };
            Ok(())
        }
        fn move_absolute(
            &mut self,
            _user: &AsynUser,
            _position: f64,
            _min_vel: f64,
            _max_vel: f64,
            _accel: f64,
        ) -> Result<(), AsynError> {
            Ok(())
        }
        fn move_relative(
            &mut self,
            _user: &AsynUser,
            _distance: f64,
            _min_vel: f64,
            _max_vel: f64,
            _accel: f64,
        ) -> Result<(), AsynError> {
            Ok(())
        }
        fn stop(&mut self, _user: &AsynUser, _accel: f64) -> Result<(), AsynError> {
            Ok(())
        }
        fn home(
            &mut self,
            _user: &AsynUser,
            _min_vel: f64,
            _max_vel: f64,
            _accel: f64,
            _forwards: bool,
        ) -> Result<(), AsynError> {
            Ok(())
        }
        fn set_position(&mut self, _user: &AsynUser, position: f64) -> Result<(), AsynError> {
            self.position = position;
            Ok(())
        }
    }

    let offline = Arc::new(AtomicBool::new(false));
    let motor: Arc<Mutex<dyn AsynMotor>> = Arc::new(Mutex::new(FlakyMotor {
        offline: offline.clone(),
        position: 3.5,
    }));
    let (poll_cmd_tx, poll_cmd_rx) = mpsc::channel(16);
    let device_state = motor_rs::device_state::new_shared_state();
    let trace = Arc::default();
    let reads = passes::bind(&device_state, &trace);
    let (io_intr_tx, _io_intr_rx) = mpsc::channel::<()>(16);

    let poll_loop = motor_rs::poll_loop::MotorPollLoop::new(
        motor_rs::poll_loop::PollerInbox {
            poll: poll_cmd_rx,
            commands: mpsc::unbounded_channel().1,
        },
        motor,
        {
            let subscribers = motor_rs::device_state::AxisSubscribers::new();
            subscribers.attach(device_state.clone(), io_intr_tx, trace);
            subscribers
        },
        Duration::from_millis(5),
        Duration::from_millis(5),
        0,
    );
    let poll_handle = tokio::spawn(poll_loop.start());

    // A healthy poll first, so the failure has a last-known status to carry
    // forward (C's parameter library keeps every field the failed poll never
    // wrote).
    poll_cmd_tx.send(PollCommand::ForcePoll).await.unwrap();
    assert!(
        !reads.wait(Duration::from_millis(500)).await.is_empty(),
        "the healthy poll must notify"
    );
    let (healthy_seq, healthy) = {
        let ds = device_state.lock().unwrap();
        let s = ds.latest_status.clone().expect("status populated");
        (s.seq, s.status)
    };
    assert!(!healthy.comms_error);
    assert_eq!(healthy.position, 3.5);

    // Cut the link. The very next poll fails.
    offline.store(true, Ordering::SeqCst);

    let posted = wait_until(Duration::from_secs(2), || {
        (reads.take().into_iter().flatten()).any(|status| status.comms_error)
    })
    .await;
    assert!(
        posted,
        "a failed poll must still post a status — otherwise MSTA never raises COMM_ERR"
    );

    let stamped = {
        let ds = device_state.lock().unwrap();
        ds.latest_status.clone().unwrap()
    };
    assert!(
        stamped.seq > healthy_seq,
        "the sequence must advance so the record actually processes the failure"
    );
    assert!(
        stamped.status.comms_error,
        "COMM_ERR is what alarm_sub turns into COMM/INVALID (motorRecord.cc:3392-3398)"
    );
    assert!(
        stamped.status.problem,
        "C sets motorStatusProblem_ alongside motorStatusCommsError_"
    );
    assert_eq!(
        stamped.status.position, 3.5,
        "fields the failed poll never wrote keep their last-known value, as C's \
         parameter library does"
    );

    let _ = poll_cmd_tx.send(PollCommand::Shutdown).await;
    let _ = poll_handle.await;
}

/// A motor whose transport is down from `offline`, failing as `failure`
/// names (C's per-driver `poll()` error label).
struct FailingMotor {
    offline: Arc<std::sync::atomic::AtomicBool>,
    failure: asyn_rs::interfaces::motor::PollFailure,
}

impl AsynMotor for FailingMotor {
    fn poll_failure(&self) -> asyn_rs::interfaces::motor::PollFailure {
        self.failure
    }
    fn poll(
        &mut self,
        _user: &asyn_rs::user::AsynUser,
        status: &mut asyn_rs::interfaces::motor::MotorStatus,
    ) -> asyn_rs::error::AsynResult<()> {
        if self.offline.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(asyn_rs::error::AsynError::Status {
                status: asyn_rs::error::AsynStatus::Timeout,
                message: "controller not responding".into(),
            });
        }
        *status = asyn_rs::interfaces::motor::MotorStatus {
            position: 3.5,
            ..Default::default()
        };
        Ok(())
    }
    fn move_absolute(
        &mut self,
        _: &asyn_rs::user::AsynUser,
        _: f64,
        _: f64,
        _: f64,
        _: f64,
    ) -> asyn_rs::error::AsynResult<()> {
        Ok(())
    }
    fn move_relative(
        &mut self,
        _: &asyn_rs::user::AsynUser,
        _: f64,
        _: f64,
        _: f64,
        _: f64,
    ) -> asyn_rs::error::AsynResult<()> {
        Ok(())
    }
    fn stop(&mut self, _: &asyn_rs::user::AsynUser, _: f64) -> asyn_rs::error::AsynResult<()> {
        Ok(())
    }
    fn home(
        &mut self,
        _: &asyn_rs::user::AsynUser,
        _: f64,
        _: f64,
        _: f64,
        _: bool,
    ) -> asyn_rs::error::AsynResult<()> {
        Ok(())
    }
    fn set_position(
        &mut self,
        _: &asyn_rs::user::AsynUser,
        _: f64,
    ) -> asyn_rs::error::AsynResult<()> {
        Ok(())
    }
}

/// A poll loop on a [`FailingMotor`], with the switch that cuts its link.
struct FailingAxis {
    offline: Arc<std::sync::atomic::AtomicBool>,
    state: motor_rs::device_state::SharedDeviceState,
    poll: mpsc::Sender<PollCommand>,
    task: tokio::task::JoinHandle<()>,
}

impl FailingAxis {
    /// The status the poller's callbacks last copied to the record.
    fn latest(&mut self) -> Option<motor_rs::device_state::StampedStatus> {
        self.state.lock().unwrap().latest_status.clone()
    }

    /// The poller thread's startup pass has posted: the bind's seed is
    /// seq 1.
    fn startup_posted(&mut self) -> bool {
        self.latest().is_some_and(|s| s.seq > 1)
    }

    async fn shutdown(self) {
        let _ = self.poll.send(PollCommand::Shutdown).await;
        let _ = self.task.await;
    }
}

fn failing_poll_loop(
    failure: asyn_rs::interfaces::motor::PollFailure,
    offline: bool,
) -> FailingAxis {
    let switch = Arc::new(std::sync::atomic::AtomicBool::new(offline));
    let motor: Arc<Mutex<dyn AsynMotor>> = Arc::new(Mutex::new(FailingMotor {
        offline: switch.clone(),
        failure,
    }));
    let (poll_cmd_tx, poll_cmd_rx) = mpsc::channel(16);
    let device_state = motor_rs::device_state::new_shared_state();
    device_state.lock().unwrap().accept();
    let (io_intr_tx, _) = mpsc::channel::<()>(1024);
    let poll_loop = motor_rs::poll_loop::MotorPollLoop::new(
        motor_rs::poll_loop::PollerInbox {
            poll: poll_cmd_rx,
            commands: mpsc::unbounded_channel().1,
        },
        motor,
        {
            let subscribers = motor_rs::device_state::AxisSubscribers::new();
            subscribers.attach(device_state.clone(), io_intr_tx, Default::default());
            subscribers
        },
        Duration::from_millis(5),
        Duration::from_millis(5),
        0,
    );
    FailingAxis {
        offline: switch,
        state: device_state,
        poll: poll_cmd_tx,
        task: tokio::spawn(poll_loop.start()),
    }
}

/// C `SMC100Axis::poll` sets only `motorStatusProblem_` at its `skip` label
/// (SMC100Driver.cpp), on an `asynMotorAxis::status_` that starts all 0: a
/// controller down from boot reads MSTA 512, not COMM_ERR|PROBLEM|DONE.
#[tokio::test]
async fn a_problem_only_failure_from_boot_raises_problem_on_a_zeroed_status() {
    use asyn_rs::interfaces::motor::PollFailure;
    let mut axis = failing_poll_loop(PollFailure::PROBLEM, true);
    assert!(wait_until(Duration::from_secs(2), || axis.startup_posted()).await);
    let status = axis.latest().unwrap().status;
    assert!(status.problem);
    assert!(
        !status.comms_error,
        "SMC100 never writes motorStatusCommsError_"
    );
    assert!(!status.done, "C status_ starts with every bit 0");
    axis.shutdown().await;
}

/// The same driver after a healthy poll keeps every field the failure did
/// not write.
#[tokio::test]
async fn a_problem_only_failure_keeps_the_last_status() {
    use asyn_rs::interfaces::motor::PollFailure;
    let mut axis = failing_poll_loop(PollFailure::PROBLEM, false);
    assert!(wait_until(Duration::from_secs(2), || axis.startup_posted()).await);
    axis.offline
        .store(true, std::sync::atomic::Ordering::SeqCst);
    assert!(
        wait_until(Duration::from_secs(2), || {
            axis.latest().is_some_and(|s| s.status.problem)
        })
        .await
    );
    let status = axis.latest().unwrap().status;
    assert!(!status.comms_error);
    assert_eq!(status.position, 3.5);
    axis.shutdown().await;
}

/// A failure raising no bit posts nothing from the background poller; a
/// forced poll still posts the unchanged status (asynMotorController.cpp
/// :217-221).
#[tokio::test]
async fn a_failure_raising_no_bit_posts_only_when_forced() {
    use asyn_rs::interfaces::motor::PollFailure;
    let mut axis = failing_poll_loop(PollFailure::NONE, false);
    assert!(wait_until(Duration::from_secs(2), || axis.latest().is_some()).await);
    tokio::time::sleep(Duration::from_millis(30)).await;
    let healthy = axis.latest().unwrap();
    axis.offline
        .store(true, std::sync::atomic::Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        axis.latest().unwrap().seq,
        healthy.seq,
        "background polls post nothing"
    );
    axis.poll.send(PollCommand::ForcePoll).await.unwrap();
    assert!(
        wait_until(Duration::from_secs(2), || {
            axis.latest().is_some_and(|s| s.seq > healthy.seq)
        })
        .await
    );
    assert_eq!(axis.latest().unwrap().status, healthy.status);
    axis.shutdown().await;
}

/// autosave pass0 restore contract. C `reboot_restore` pass0 writes fields
/// with the static `dbPutString` BEFORE `init_record` runs (`initHookAfterInitDevSup`
/// precedes `initDatabase` in iocInit), so a restored VAL is a plain field
/// write: per-record device init clears the parked put (`clear_last_write`)
/// and the Startup readback's RSTM decision adopts the restored DVAL via
/// `SetPosition` (redefine). A restore must never dispatch a move — the
/// regression dispatched the restored VAL as a real move on the first
/// status pass, and on a fast axis the Startup readback then caught the
/// move mid-flight and synced the instantaneous position into VAL/DVAL.
#[tokio::test]
async fn pass0_restored_dval_is_not_a_move_command() {
    use asyn_rs::error::AsynError;
    use asyn_rs::interfaces::motor::MotorStatus;
    use asyn_rs::user::AsynUser;
    use std::sync::atomic::{AtomicU32, Ordering};

    struct SpyMotor {
        position: f64,
        moves: Arc<AtomicU32>,
        loads: Arc<AtomicU32>,
    }
    impl AsynMotor for SpyMotor {
        fn poll_failure(&self) -> asyn_rs::interfaces::motor::PollFailure {
            asyn_rs::interfaces::motor::PollFailure::PROBLEM_AND_COMMS_ERROR
        }
        fn poll(&mut self, _user: &AsynUser, status: &mut MotorStatus) -> Result<(), AsynError> {
            *status = MotorStatus {
                position: self.position,
                done: true,
                ..MotorStatus::default()
            };
            Ok(())
        }
        fn move_absolute(
            &mut self,
            _user: &AsynUser,
            position: f64,
            _min_vel: f64,
            _max_vel: f64,
            _accel: f64,
        ) -> Result<(), AsynError> {
            self.moves.fetch_add(1, Ordering::SeqCst);
            self.position = position;
            Ok(())
        }
        fn move_relative(
            &mut self,
            _user: &AsynUser,
            _distance: f64,
            _min_vel: f64,
            _max_vel: f64,
            _accel: f64,
        ) -> Result<(), AsynError> {
            Ok(())
        }
        fn stop(&mut self, _user: &AsynUser, _accel: f64) -> Result<(), AsynError> {
            Ok(())
        }
        fn home(
            &mut self,
            _user: &AsynUser,
            _min_vel: f64,
            _max_vel: f64,
            _accel: f64,
            _forwards: bool,
        ) -> Result<(), AsynError> {
            Ok(())
        }
        fn set_position(&mut self, _user: &AsynUser, position: f64) -> Result<(), AsynError> {
            self.loads.fetch_add(1, Ordering::SeqCst);
            self.position = position;
            Ok(())
        }
    }

    let moves = Arc::new(AtomicU32::new(0));
    let loads = Arc::new(AtomicU32::new(0));
    let motor: Arc<Mutex<dyn AsynMotor>> = Arc::new(Mutex::new(SpyMotor {
        position: 0.0,
        moves: moves.clone(),
        loads: loads.clone(),
    }));
    let mut setup = make_builder(motor.clone()).build();

    // pass0 restore: a silent field write, before device init. Autosave
    // restores DVAL (motor_positions.req); a VAL write would stay raw,
    // as C special() does not cascade it.
    setup
        .record
        .put_field("DVAL", EpicsValue::Double(1.5))
        .unwrap();
    // C init_record era: initial readback seed + parked-put clear.
    setup.device_support.init(&mut setup.record).unwrap();
    // First status pass = Startup -> initial_readback -> RSTM restore.
    setup.record.process().unwrap();
    setup.device_support.write(&mut setup.record).unwrap();
    setup.poll_loop.run_queued_commands();

    assert_eq!(
        setup.record.pos.val, 1.5,
        "restored DVAL survives init and the Startup pass"
    );
    assert_eq!(setup.record.pos.dval, 1.5);
    assert!(setup.record.stat.dmov != 0, "restored axis comes up done");
    assert_eq!(
        moves.load(Ordering::SeqCst),
        0,
        "a pass0-restored DVAL must never dispatch a move"
    );
    assert_eq!(
        loads.load(Ordering::SeqCst),
        1,
        "RSTM NearZero adopts the autosaved DVAL via SetPosition"
    );
    let user = AsynUser::new(0);
    let st = polled(&mut *motor.lock().unwrap(), &user);
    assert_eq!(
        st.position, 1500.0,
        "the controller was redefined, not moved"
    );
}

/// A driver stuck in a serial/TCP read timeout blocks only the axis's
/// `motorPoller` thread, as C runs `asynMotorPoller` and the port's queued
/// writes on their own threads: neither its poll nor a move the record
/// queues may hold the IOC runtime, here a single-threaded one.
#[tokio::test]
async fn blocked_driver_does_not_stall_the_ioc_runtime() {
    use asyn_rs::error::AsynError;
    use asyn_rs::interfaces::motor::MotorStatus;
    use asyn_rs::user::AsynUser;
    use std::sync::atomic::{AtomicBool, Ordering};

    const TIMEOUT: Duration = Duration::from_secs(2);

    /// Answers until `dead`, then every call sits out a read timeout.
    struct DeadLine {
        dead: Arc<AtomicBool>,
    }
    impl DeadLine {
        fn wire(&self) {
            if self.dead.load(Ordering::SeqCst) {
                std::thread::sleep(TIMEOUT);
            }
        }
    }
    impl AsynMotor for DeadLine {
        fn poll_failure(&self) -> asyn_rs::interfaces::motor::PollFailure {
            asyn_rs::interfaces::motor::PollFailure::PROBLEM_AND_COMMS_ERROR
        }
        fn poll(&mut self, _user: &AsynUser, status: &mut MotorStatus) -> Result<(), AsynError> {
            self.wire();
            *status = MotorStatus {
                done: true,
                ..MotorStatus::default()
            };
            Ok(())
        }
        fn move_absolute(
            &mut self,
            _user: &AsynUser,
            _position: f64,
            _min_vel: f64,
            _max_vel: f64,
            _accel: f64,
        ) -> Result<(), AsynError> {
            self.wire();
            Ok(())
        }
        fn move_relative(
            &mut self,
            _user: &AsynUser,
            _distance: f64,
            _min_vel: f64,
            _max_vel: f64,
            _accel: f64,
        ) -> Result<(), AsynError> {
            Ok(())
        }
        fn stop(&mut self, _user: &AsynUser, _accel: f64) -> Result<(), AsynError> {
            self.wire();
            Ok(())
        }
        fn home(
            &mut self,
            _user: &AsynUser,
            _min_vel: f64,
            _max_vel: f64,
            _accel: f64,
            _forwards: bool,
        ) -> Result<(), AsynError> {
            self.wire();
            Ok(())
        }
        fn set_position(&mut self, _user: &AsynUser, _position: f64) -> Result<(), AsynError> {
            self.wire();
            Ok(())
        }
    }

    let dead = Arc::new(AtomicBool::new(false));
    let motor: Arc<Mutex<dyn AsynMotor>> = Arc::new(Mutex::new(DeadLine { dead: dead.clone() }));
    let mut setup = make_builder(motor).build();
    setup.device_support.init(&mut setup.record).unwrap();
    setup.record.process().unwrap();
    setup.device_support.write(&mut setup.record).unwrap();
    let poll_handle = tokio::spawn(setup.poll_loop.start());
    dead.store(true, Ordering::SeqCst);

    // Poll path: the forced poll sits in the dead read while the runtime
    // keeps serving its other tasks.
    setup
        .poll_cmd_tx
        .send(PollCommand::ForcePoll)
        .await
        .unwrap();
    let start = Instant::now();
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert!(
        start.elapsed() < TIMEOUT / 2,
        "a blocked poll held the runtime for {:?}",
        start.elapsed()
    );

    // Command path: the record pass that dispatches a move returns at once.
    setup
        .record
        .put_field("VAL", EpicsValue::Double(1.0))
        .unwrap();
    setup.record.process().unwrap();
    let start = Instant::now();
    setup.device_support.write(&mut setup.record).unwrap();
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert!(
        start.elapsed() < TIMEOUT / 2,
        "a blocked move held the record pass and runtime for {:?}",
        start.elapsed()
    );

    dead.store(false, Ordering::SeqCst);
    let _ = setup.poll_cmd_tx.send(PollCommand::Shutdown).await;
    let _ = poll_handle.await;
}

/// The IOC starts the poller from the iocsh thread, as C's `startPoller`
/// does, and the thread exists on return: it serves a move and keeps its
/// timed polls while the caller's current-thread runtime is blocked.
#[tokio::test]
async fn a_poller_started_from_the_shell_runs_while_the_runtime_blocks() {
    use asyn_rs::error::AsynError;
    use asyn_rs::interfaces::motor::MotorStatus;
    use asyn_rs::user::AsynUser;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    #[derive(Default)]
    struct Seen {
        polls: AtomicUsize,
        moved: AtomicBool,
    }
    struct Counting(Arc<Seen>);
    impl AsynMotor for Counting {
        fn poll_failure(&self) -> asyn_rs::interfaces::motor::PollFailure {
            asyn_rs::interfaces::motor::PollFailure::PROBLEM_AND_COMMS_ERROR
        }
        fn poll(&mut self, _user: &AsynUser, status: &mut MotorStatus) -> Result<(), AsynError> {
            self.0.polls.fetch_add(1, Ordering::SeqCst);
            *status = MotorStatus {
                done: true,
                ..MotorStatus::default()
            };
            Ok(())
        }
        fn move_absolute(
            &mut self,
            _user: &AsynUser,
            _position: f64,
            _min_vel: f64,
            _max_vel: f64,
            _accel: f64,
        ) -> Result<(), AsynError> {
            self.0.moved.store(true, Ordering::SeqCst);
            Ok(())
        }
        fn move_relative(
            &mut self,
            _user: &AsynUser,
            _distance: f64,
            _min_vel: f64,
            _max_vel: f64,
            _accel: f64,
        ) -> Result<(), AsynError> {
            Ok(())
        }
        fn stop(&mut self, _user: &AsynUser, _accel: f64) -> Result<(), AsynError> {
            Ok(())
        }
        fn home(
            &mut self,
            _user: &AsynUser,
            _min_vel: f64,
            _max_vel: f64,
            _accel: f64,
            _forwards: bool,
        ) -> Result<(), AsynError> {
            Ok(())
        }
        fn set_position(&mut self, _user: &AsynUser, _position: f64) -> Result<(), AsynError> {
            Ok(())
        }
    }

    let seen = Arc::new(Seen::default());
    let motor: Arc<Mutex<dyn AsynMotor>> = Arc::new(Mutex::new(Counting(seen.clone())));
    let mut setup = make_builder(motor).build();
    setup.device_support.init(&mut setup.record).unwrap();
    setup.record.process().unwrap();
    setup.device_support.write(&mut setup.record).unwrap();

    let poll_loop = setup.poll_loop;
    std::thread::spawn(move || {
        drop(poll_loop.start());
    })
    .join()
    .unwrap();

    setup
        .record
        .put_field("VAL", EpicsValue::Double(1.0))
        .unwrap();
    setup.record.process().unwrap();
    setup.device_support.write(&mut setup.record).unwrap();
    setup
        .poll_cmd_tx
        .send(PollCommand::ForcePoll)
        .await
        .unwrap();

    let deadline = Instant::now() + Duration::from_secs(5);
    while (!seen.moved.load(Ordering::SeqCst) || seen.polls.load(Ordering::SeqCst) < 5)
        && Instant::now() < deadline
    {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        seen.moved.load(Ordering::SeqCst),
        "the move never reached the driver"
    );
    assert!(
        seen.polls.load(Ordering::SeqCst) >= 5,
        "timed polls stopped at {}",
        seen.polls.load(Ordering::SeqCst)
    );
    let _ = setup.poll_cmd_tx.send(PollCommand::Shutdown).await;
}

/// MSTA a motorSim axis reads at rest, through init + process: C reads 130
/// (Done | Home) at the home position and 2 (Done) off it
/// (motorSimDriver.cpp:471-477, bit order asynMotorController.cpp:85-99).
#[tokio::test]
async fn sim_motor_msta_at_rest_matches_motor_sim_driver() {
    for (start, c_msta) in [(0.0, 130u32), (5.0, 2)] {
        let motor: Arc<Mutex<dyn AsynMotor>> =
            Arc::new(Mutex::new(SimMotor::new().with_position(start)));
        let mut setup = MotorBuilder::new(motor)
            .poll_interval(Duration::from_millis(5))
            .build();
        setup.device_support.init(&mut setup.record).unwrap();
        let poll_handle = tokio::spawn(setup.poll_loop.start());

        let msta = |rec: &motor_rs::record::MotorRecord| match rec.get_field("MSTA") {
            Some(EpicsValue::ULong(v)) => v,
            other => panic!("MSTA read {other:?}"),
        };
        let record_ref = &mut setup.record;
        let ds_ref = &mut setup.device_support;
        wait_until(Duration::from_secs(2), || {
            record_ref.process().unwrap();
            ds_ref.write(record_ref).unwrap();
            msta(record_ref) == c_msta
        })
        .await;
        assert_eq!(msta(&setup.record), c_msta, "MSTA at position {start}");

        let _ = setup.poll_cmd_tx.send(PollCommand::Shutdown).await;
        let _ = poll_handle.await;
    }
}

/// C init_record reads the poller's startup poll once the poller thread has
/// run it (asynMotorController.cpp:505-525, :630).
#[tokio::test]
async fn init_reads_the_startup_poll_once_the_poller_ran_it() {
    let motor: Arc<Mutex<dyn AsynMotor>> = Arc::new(Mutex::new(SimMotor::new().with_position(5.0)));
    let mut setup = MotorBuilder::new(motor).build();
    setup.poll_loop.run_startup_pass();
    setup.device_support.init(&mut setup.record).unwrap();
    assert_eq!(
        setup.record.get_field("MSTA"),
        Some(EpicsValue::ULong(2)),
        "RA_DONE from the startup poll"
    );
}

/// The poll runs on the poller's own thread (asynMotorController.cpp:572-575,
/// :630), so an init_record ahead of it reads `status_` as the axis
/// constructor left it: all 0 (asynMotorAxis.cpp:46).
#[tokio::test]
async fn init_ahead_of_the_poller_reads_the_constructor_status() {
    let motor: Arc<Mutex<dyn AsynMotor>> = Arc::new(Mutex::new(SimMotor::new().with_position(5.0)));
    let mut setup = MotorBuilder::new(motor).build();
    setup.device_support.init(&mut setup.record).unwrap();
    assert_eq!(setup.record.get_field("MSTA"), Some(EpicsValue::ULong(0)));
}

/// C `asynMotorPoller` polls on its period from start-up and nothing a
/// record does stops it (asynMotorController.cpp:615-696): with no request
/// ever sent, a move only the driver knows of still reaches the record.
#[tokio::test]
async fn the_axis_polls_with_no_record_request() {
    use motor_rs::device_state::{AxisSubscribers, new_shared_state};
    let motor: Arc<Mutex<dyn AsynMotor>> = Arc::new(Mutex::new(SimMotor::new()));
    let (poll_cmd_tx, poll_cmd_rx) = mpsc::channel(16);
    let subscribers = AxisSubscribers::new();
    let state = new_shared_state();
    let trace = Arc::default();
    let reads = passes::bind(&state, &trace);
    let (io_intr_tx, _io_intr) = mpsc::channel::<()>(64);
    subscribers.attach(state.clone(), io_intr_tx, trace);
    let poll_loop = motor_rs::poll_loop::MotorPollLoop::new(
        motor_rs::poll_loop::PollerInbox {
            poll: poll_cmd_rx,
            commands: mpsc::unbounded_channel().1,
        },
        motor.clone(),
        subscribers,
        Duration::from_millis(5),
        Duration::from_millis(5),
        0,
    );
    let poll_handle = tokio::spawn(poll_loop.start());
    assert!(
        !reads.wait(Duration::from_secs(10)).await.is_empty(),
        "the startup pass's post"
    );

    motor
        .lock()
        .unwrap()
        .move_absolute(&asyn_rs::user::AsynUser::new(0), 0.2, 0.0, 100.0, 100.0)
        .unwrap();
    let posted = reads.wait(Duration::from_secs(10)).await;
    assert!(!posted.is_empty(), "a timed poll saw the move");
    let _ = poll_cmd_tx.send(PollCommand::Shutdown).await;
    let _ = poll_handle.await;
}

/// C `motorUpdateStatus_` polls on the port thread (asynMotorController.cpp:
/// 217-222) and leaves the poller alone: only `wakeupPoller` — a move, here —
/// reloads `forcedFastPolls_` (:636-642). A ForcePoll on an idle axis is one
/// poll, and the idle period continues; a Wakeup reloads them as a move does.
#[tokio::test(flavor = "multi_thread")]
async fn force_poll_does_not_reload_the_forced_fast_polls() {
    use asyn_rs::error::AsynError;
    use asyn_rs::interfaces::motor::MotorStatus;
    use asyn_rs::user::AsynUser;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Counting(Arc<AtomicUsize>);
    impl AsynMotor for Counting {
        fn poll_failure(&self) -> asyn_rs::interfaces::motor::PollFailure {
            asyn_rs::interfaces::motor::PollFailure::PROBLEM_AND_COMMS_ERROR
        }
        fn poll(&mut self, _user: &AsynUser, status: &mut MotorStatus) -> Result<(), AsynError> {
            self.0.fetch_add(1, Ordering::SeqCst);
            *status = MotorStatus {
                done: true,
                ..MotorStatus::default()
            };
            Ok(())
        }
        fn move_absolute(
            &mut self,
            _user: &AsynUser,
            _position: f64,
            _min_vel: f64,
            _max_vel: f64,
            _accel: f64,
        ) -> Result<(), AsynError> {
            Ok(())
        }
        fn move_relative(
            &mut self,
            _user: &AsynUser,
            _distance: f64,
            _min_vel: f64,
            _max_vel: f64,
            _accel: f64,
        ) -> Result<(), AsynError> {
            Ok(())
        }
        fn stop(&mut self, _user: &AsynUser, _accel: f64) -> Result<(), AsynError> {
            Ok(())
        }
        fn home(
            &mut self,
            _user: &AsynUser,
            _min_vel: f64,
            _max_vel: f64,
            _accel: f64,
            _forwards: bool,
        ) -> Result<(), AsynError> {
            Ok(())
        }
        fn set_position(&mut self, _user: &AsynUser, _position: f64) -> Result<(), AsynError> {
            Ok(())
        }
    }

    let polls = Arc::new(AtomicUsize::new(0));
    let motor: Arc<Mutex<dyn AsynMotor>> = Arc::new(Mutex::new(Counting(polls.clone())));
    let (poll_cmd_tx, poll_cmd_rx) = mpsc::channel(16);
    let (motor_cmd_tx, motor_cmd_rx) = mpsc::unbounded_channel();
    let (io_intr_tx, mut io_intr_rx) = mpsc::channel::<()>(64);
    tokio::spawn(async move { while io_intr_rx.recv().await.is_some() {} });
    let poll_loop = motor_rs::poll_loop::MotorPollLoop::new(
        motor_rs::poll_loop::PollerInbox {
            poll: poll_cmd_rx,
            commands: motor_cmd_rx,
        },
        motor,
        {
            let subscribers = motor_rs::device_state::AxisSubscribers::new();
            subscribers.attach(
                motor_rs::device_state::new_shared_state(),
                io_intr_tx,
                Default::default(),
            );
            subscribers
        },
        Duration::from_millis(10),
        Duration::from_secs(60),
        5,
    );
    let poll_handle = tokio::spawn(poll_loop.start());
    let settled = |n: usize| {
        let polls = polls.clone();
        async move {
            let seen = || polls.load(Ordering::SeqCst);
            assert!(wait_until(Duration::from_secs(10), || seen() >= n).await);
            // Past any forced fast poll still to come, 10 ms apart.
            tokio::time::sleep(Duration::from_millis(400)).await;
            assert_eq!(seen(), n, "polls so far");
        }
    };
    // Startup: the startup poll and its 5 forced fast polls.
    settled(6).await;

    poll_cmd_tx.send(PollCommand::ForcePoll).await.unwrap();
    settled(7).await;

    motor_cmd_tx
        .send(
            motor_rs::poll_loop::CommandBatch {
                trace: Arc::new(motor_rs::trace::MotorTrace::new("forcePollFast", 0)),
                record: Some(0),
                commands: vec![MotorCommand::MoveAbsolute {
                    position: 10.0,
                    min_velocity: 0.0,
                    velocity: 1.0,
                    acceleration: 1.0,
                }],
            }
            .into(),
        )
        .unwrap();
    // The move wakes the poller: one poll and 5 forced fast polls.
    settled(13).await;

    // A driver thread's wakeupPoller does the same.
    poll_cmd_tx.send(PollCommand::Wakeup).await.unwrap();
    settled(19).await;

    let _ = poll_cmd_tx.send(PollCommand::Shutdown).await;
    let _ = poll_handle.await;
}

/// C init_record (717-718): set_dial_highlimit sends nothing when the
/// device refuses the limit, and HLM follows DHLM as the device reset it
/// (devMM4000.cc:316-336).
#[tokio::test]
async fn init_dial_limit_follows_a_refusing_device() {
    use motor_rs::device_support::{BuildTrans, DialLimitReset, LimitBuild};
    struct Clamp;
    impl BuildTrans for Clamp {
        fn limit(&self, high: bool, raw: f64, mres: f64) -> LimitBuild {
            if high && raw * mres > 80.0 {
                LimitBuild::Refused(Some(DialLimitReset::Dhlm(80.0)))
            } else {
                LimitBuild::Built
            }
        }
    }
    let motor: Arc<Mutex<dyn AsynMotor>> = Arc::new(Mutex::new(SimMotor::new()));
    let setup = make_builder(motor).build_axis();
    let mut support = setup.axis.with_build_trans(Arc::new(Clamp)).attach();
    let mut rec = motor_rs::MotorRecord::new();
    rec.conv.mres = 0.001;
    rec.limits.dhlm = 100.0;
    rec.limits.dllm = -100.0;
    rec.limits.hlm = 100.0;
    rec.limits.llm = -100.0;
    support.init(&mut rec).unwrap();
    assert_eq!((rec.limits.dhlm, rec.limits.hlm), (80.0, 80.0));
    assert_eq!((rec.limits.dllm, rec.limits.llm), (-100.0, -100.0));
}

/// motordevCom.cc:327-343: init_record's commands go through the device's
/// build_trans too, so a device that defers sees them.
#[tokio::test]
async fn init_commands_reach_the_device_build_trans() {
    use motor_rs::device_support::BuildTrans;
    #[derive(Default)]
    struct Seen(Mutex<Vec<MotorCommand>>);
    impl BuildTrans for Seen {
        fn defers(&self, command: &MotorCommand) -> bool {
            self.0.lock().unwrap().push(command.clone());
            false
        }
    }
    let seen = Arc::new(Seen::default());
    let motor: Arc<Mutex<dyn AsynMotor>> = Arc::new(Mutex::new(SimMotor::new()));
    let setup = make_builder(motor).build_axis();
    let mut support = setup.axis.with_build_trans(seen.clone()).attach();
    let mut rec = motor_rs::MotorRecord::new();
    rec.conv.mres = 0.001;
    support.init(&mut rec).unwrap();
    let seen = seen.0.lock().unwrap();
    assert!(
        seen.iter()
            .any(|c| matches!(c, MotorCommand::InitEncoderRatio { .. })),
        "{seen:?}"
    );
}

/// C set_dial_highlimit/lowlimit (4236-4325), run by init_record (717-718),
/// re-derive HLM/LLM and set RHLM/RLLM to the raw limit they sent whether
/// or not the device built it.
#[tokio::test]
async fn init_dial_limits_always_set_user_and_raw_limits() {
    use motor_rs::device_support::{BuildTrans, DialLimitReset, LimitBuild};
    struct Clamp(bool);
    impl BuildTrans for Clamp {
        fn limit(&self, high: bool, _: f64, _: f64) -> LimitBuild {
            if self.0 && high {
                LimitBuild::Refused(Some(DialLimitReset::Dhlm(80.0)))
            } else {
                LimitBuild::Built
            }
        }
    }
    for refuse in [false, true] {
        let motor: Arc<Mutex<dyn AsynMotor>> = Arc::new(Mutex::new(SimMotor::new()));
        let setup = make_builder(motor).build_axis();
        let mut support = setup
            .axis
            .with_build_trans(Arc::new(Clamp(refuse)))
            .attach();
        let mut rec = motor_rs::MotorRecord::new();
        rec.conv.mres = 0.001;
        rec.limits.dhlm = 100.0;
        rec.limits.dllm = -100.0;
        support.init(&mut rec).unwrap();
        let hlm = if refuse { 80.0 } else { 100.0 };
        assert_eq!((rec.limits.hlm, rec.limits.llm), (hlm, -100.0), "{refuse}");
        assert_eq!(
            (rec.limits.rhlm, rec.limits.rllm),
            (100.0 / 0.001, -100.0 / 0.001),
            "{refuse}"
        );
    }
}

/// Review probe: a dbLoadRecords HLM is a raw field() write in C; init_record
/// 717 then re-derives HLM from DHLM (set_dial_highlimit 4255-4258).
#[tokio::test]
async fn review_loaded_hlm_yields_to_dhlm_at_init() {
    use epics_base_rs::server::record::Record;
    let motor: Arc<Mutex<dyn AsynMotor>> = Arc::new(Mutex::new(SimMotor::new()));
    let setup = make_builder(motor).build_axis();
    let mut support = setup.axis.attach();
    let mut rec = motor_rs::MotorRecord::new();
    rec.put_field("MRES", EpicsValue::Double(0.001)).unwrap();
    rec.put_field("DHLM", EpicsValue::Double(20.0)).unwrap();
    rec.put_field("HLM", EpicsValue::Double(50.0)).unwrap();
    rec.init_record(1).unwrap();
    support.init(&mut rec).unwrap();
    assert_eq!((rec.limits.dhlm, rec.limits.hlm), (20.0, 20.0));
}

/// Review probe: dbLoadRecords OFF then DIR are raw field() writes in C;
/// init_record does not recompute OFF.
#[tokio::test]
async fn review_loaded_off_survives_loaded_dir() {
    use epics_base_rs::server::record::Record;
    let motor: Arc<Mutex<dyn AsynMotor>> = Arc::new(Mutex::new(SimMotor::new()));
    let setup = make_builder(motor).build_axis();
    let mut support = setup.axis.attach();
    let mut rec = motor_rs::MotorRecord::new();
    rec.put_field("MRES", EpicsValue::Double(0.001)).unwrap();
    rec.put_field("OFF", EpicsValue::Double(5.0)).unwrap();
    rec.put_field("DIR", EpicsValue::Short(1)).unwrap();
    rec.init_record(1).unwrap();
    support.init(&mut rec).unwrap();
    assert_eq!(rec.pos.off, 5.0);
}

/// Review probe: no device init reaches 717 (C returns at 646-652), so a
/// loaded HLM stays as loaded even when DIR is loaded after it.
#[test]
fn review_loaded_hlm_kept_without_device_init() {
    use epics_base_rs::server::record::Record;
    let mut rec = motor_rs::MotorRecord::new();
    rec.put_field("MRES", EpicsValue::Double(0.001)).unwrap();
    rec.put_field("DHLM", EpicsValue::Double(20.0)).unwrap();
    rec.put_field("HLM", EpicsValue::Double(50.0)).unwrap();
    rec.put_field("DIR", EpicsValue::Short(1)).unwrap();
    rec.put_field("OFF", EpicsValue::Double(1.0)).unwrap();
    rec.init_record(1).unwrap();
    assert_eq!((rec.limits.dhlm, rec.limits.hlm), (20.0, 50.0));
}
