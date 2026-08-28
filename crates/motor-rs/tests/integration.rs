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
            rec.limits.lvio = false;
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

#[tokio::test]
async fn test_full_move_via_mailbox() {
    let motor: Arc<Mutex<dyn AsynMotor>> = Arc::new(Mutex::new(SimMotor::new()));
    let mut setup = make_builder(motor).build();

    // Init device support
    setup.device_support.init(&mut setup.record).unwrap();

    // Spawn poll loop
    let poll_handle = tokio::spawn(setup.poll_loop.run());

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

    assert!(!setup.record.stat.dmov);

    // Wait for DMOV=true with polling
    let record_ref = &mut setup.record;
    let ds_ref = &mut setup.device_support;
    let reached = wait_until(Duration::from_secs(2), || {
        // Process record to pick up device updates
        record_ref.process().unwrap();
        ds_ref.write(record_ref).unwrap();
        record_ref.stat.dmov
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
    let poll_handle = tokio::spawn(setup.poll_loop.run());

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

    assert!(!setup.record.stat.dmov);

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
        record_ref.stat.dmov
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
            rec.limits.lvio = false;
            rec.vel.velo = 100000.0; // very fast
            rec.vel.accl = 0.5;
            rec.vel.bvel = 5.0;
            rec.vel.bacc = 0.5;
            rec.stat.msta = MstaFlags::DONE;
            rec.timing.dly = 0.05; // 50ms delay
        })
        .build();

    setup.device_support.init(&mut setup.record).unwrap();
    let poll_handle = tokio::spawn(setup.poll_loop.run());

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
        record_ref.stat.dmov
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
    let motor: Arc<Mutex<dyn AsynMotor>> = Arc::new(Mutex::new(SimMotor::new()));
    let mut setup = make_builder(motor)
        .configure_record(|rec| {
            rec.conv.mres = 0.001;
            rec.limits.dhlm = 100.0;
            rec.limits.dllm = -100.0;
            rec.limits.hlm = 100.0;
            rec.limits.llm = -100.0;
            rec.limits.lvio = false;
            rec.vel.velo = 100000.0;
            rec.vel.accl = 0.5;
            rec.vel.bvel = 100000.0;
            rec.vel.bacc = 0.5;
            rec.stat.msta = MstaFlags::DONE;
            rec.retry.bdst = 1.0; // positive backlash
        })
        .build();

    setup.device_support.init(&mut setup.record).unwrap();
    let poll_handle = tokio::spawn(setup.poll_loop.run());

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

    assert!(!setup.record.stat.dmov);

    // Wait for DMOV
    let record_ref = &mut setup.record;
    let ds_ref = &mut setup.device_support;
    let reached = wait_until(Duration::from_secs(2), || {
        record_ref.process().unwrap();
        ds_ref.write(record_ref).unwrap();
        record_ref.stat.dmov
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
    let (io_intr_tx, mut io_intr_rx) = mpsc::channel::<()>(16);

    let poll_loop = motor_rs::poll_loop::MotorPollLoop::new(
        poll_cmd_rx,
        io_intr_tx,
        motor,
        device_state.clone(),
        Duration::from_millis(5),
        Duration::from_millis(5),
        0,
    );

    let poll_handle = tokio::spawn(poll_loop.run());

    // Start polling
    poll_cmd_tx.send(PollCommand::StartPolling).await.unwrap();

    // Wait for at least one io_intr notification
    let got_notification =
        tokio::time::timeout(Duration::from_millis(500), io_intr_rx.recv()).await;
    assert!(
        got_notification.is_ok(),
        "should receive io_intr from poll loop"
    );

    // Verify status was written
    {
        let ds = device_state.lock().unwrap();
        assert!(ds.latest_status.is_some(), "status should be populated");
    }

    // Stop polling
    poll_cmd_tx.send(PollCommand::StopPolling).await.unwrap();

    // Drain any in-flight notifications
    tokio::time::sleep(Duration::from_millis(20)).await;
    while io_intr_rx.try_recv().is_ok() {}

    // Verify no more notifications arrive
    let no_notification = tokio::time::timeout(Duration::from_millis(50), io_intr_rx.recv()).await;
    assert!(
        no_notification.is_err(),
        "should not receive notifications after StopPolling"
    );

    // Shutdown
    poll_cmd_tx.send(PollCommand::Shutdown).await.unwrap();
    let result = tokio::time::timeout(Duration::from_secs(1), poll_handle).await;
    assert!(result.is_ok(), "poll loop should terminate after Shutdown");
}

/// R61: a zero idle poll interval is event-only, never a busy-spin.
/// C `asynMotorController::asynMotorPoller` (asynMotorController.cpp:633-634)
/// treats `idlePollPeriod_ == 0` as "block on the event, no timed poll". The
/// Rust loop guards the `sleep(interval)` select arm on `!interval.is_zero()`,
/// so with a settled (done, not-moving) motor StartPolling triggers exactly
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
        poll_cmd_rx,
        io_intr_tx,
        motor,
        device_state.clone(),
        Duration::from_millis(5),
        Duration::ZERO,
        0,
    );
    let poll_handle = tokio::spawn(poll_loop.run());

    // Continuously drain io_intr so a (would-be) busy-spin is NOT throttled by
    // channel backpressure — without draining, a spin stalls on the full
    // channel and masks itself.
    tokio::spawn(async move { while io_intr_rx.recv().await.is_some() {} });

    poll_cmd_tx.send(PollCommand::StartPolling).await.unwrap();

    // Give any erroneous busy-spin a 100ms window to accumulate polls.
    tokio::time::sleep(Duration::from_millis(100)).await;

    // seq starts at 1 (init), +1 for the single StartPolling poll → 2.
    // A busy-spin would drive it into the hundreds/thousands.
    let seq = device_state
        .lock()
        .unwrap()
        .latest_status
        .as_ref()
        .map(|s| s.seq)
        .unwrap_or(0);
    assert_eq!(
        seq, 2,
        "zero idle interval must poll once on StartPolling then block (event-only), got seq {seq}"
    );

    let _ = poll_cmd_tx.send(PollCommand::Shutdown).await;
    let _ = poll_handle.await;
}

/// Idle change-detection: a stationary axis polled at a NON-zero idle interval
/// must not re-post on every poll. C `asynMotorAxis::callParamCallbacks`
/// (asynMotorAxis.cpp:316-322) fires the status callback — which drives the
/// record's process() — only when `statusChanged_` is set; on an unchanging
/// idle poll C posts nothing. A *forced* poll (StartPolling, the analogue of
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
        poll_cmd_rx,
        io_intr_tx,
        motor,
        device_state.clone(),
        Duration::from_millis(5),
        Duration::from_millis(5),
        0,
    );
    let poll_handle = tokio::spawn(poll_loop.run());

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

    // StartPolling forces one post → seq 1→2.
    poll_cmd_tx.send(PollCommand::StartPolling).await.unwrap();
    tokio::time::sleep(Duration::from_millis(80)).await;
    assert_eq!(
        seq(&device_state),
        2,
        "a stationary axis must not re-post on every idle poll (change-gated)"
    );

    // A second StartPolling is a FORCED poll → re-posts even though the status
    // is byte-identical → seq 2→3.
    poll_cmd_tx.send(PollCommand::StartPolling).await.unwrap();
    tokio::time::sleep(Duration::from_millis(80)).await;
    assert_eq!(
        seq(&device_state),
        3,
        "a forced poll must post even when status is unchanged (STUP/GET_INFO ack)"
    );

    let _ = poll_cmd_tx.send(PollCommand::Shutdown).await;
    let _ = poll_handle.await;
}

/// Held-jog resume must survive the idle-poll change-gate. The operator holds
/// JOGF during a positional move; on a close-enough completion the jog resumes
/// on a follow-up record pass (the Idle-arm `dispatch_latent_collection`). C
/// strands this (special() arms MIP_JOG_REQ only at mip==MIP_DONE,
/// motorRecord.cc:3045, so a button pressed during a move is never armed); Rust
/// preserves the resume as a deliberate divergence. With the change-gate, a
/// now-stationary axis no longer notifies on an unchanged poll, so the resume
/// would strand unless `finalize_motion` requests one forced poll.
///
/// This test drives the record ONLY on `io_intr` — the production wiring —
/// unlike the unit test `close_enough_defers_held_jog_to_next_idle_poll`, which
/// calls `check_completion()` directly and so cannot observe a missing forced
/// poll. Without the `finalize_motion` request_poll the settle poll is
/// suppressed, no `io_intr` fires, and the axis stays Idle until the deadline.
// current_thread is required: the synchronous drain below is race-free only
// because the poll-loop task cannot run during it (no await). On a multi_thread
// runtime the forced poll could fire concurrently mid-drain and be drained,
// flaking the assertion.
#[tokio::test(flavor = "current_thread")]
async fn test_held_jog_resumes_through_poll_loop_after_close_enough() {
    let motor: Arc<Mutex<dyn AsynMotor>> = Arc::new(Mutex::new(SimMotor::new()));
    let mut setup = make_builder(motor).build();

    setup.device_support.init(&mut setup.record).unwrap();
    // Drive the record only when the poll loop fires io_intr (production wiring).
    let mut io_intr_rx = setup
        .device_support
        .io_intr_receiver()
        .expect("device support owns the io_intr receiver until iocInit wiring");
    let poll_handle = tokio::spawn(setup.poll_loop.run());

    // Startup pass.
    setup.record.process().unwrap();
    setup.device_support.write(&mut setup.record).unwrap();

    // Move at a moderate speed (≈40ms over several 5ms polls) so the loop
    // traverses genuine moving-phase notifications before completion. Strand
    // detection does NOT depend on this speed — the Phase-2 drain below is what
    // prevents a buffered moving-phase io_intr from masking the strand, so a
    // near-instant move detects it just as reliably. Lands within RDBD of the
    // target (close-enough on arrival).
    setup.record.vel.velo = 50.0;
    setup.record.retry.rdbd = 0.5;
    setup
        .record
        .put_field("VAL", EpicsValue::Double(2.0))
        .unwrap();
    setup.record.process().unwrap();
    setup.device_support.write(&mut setup.record).unwrap();

    // Operator holds JOGF DURING the move (mip == MOVE, not DONE): MIP_JOG_REQ
    // is never armed — exactly the case C strands.
    setup.record.ctrl.jogf = true;

    let deadline = Instant::now() + Duration::from_secs(2);

    // Phase 1: process io_intr until the move completes (finalize runs; the jog
    // is NOT fired in the completion pass).
    let mut completed = false;
    while Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(100), io_intr_rx.recv()).await {
            Ok(Some(())) => {
                setup.record.process().unwrap();
                setup.device_support.write(&mut setup.record).unwrap();
                if setup.record.stat.dmov && setup.record.stat.phase == MotionPhase::Idle {
                    completed = true;
                    break;
                }
            }
            Ok(None) => break,
            Err(_) => continue,
        }
    }
    assert!(completed, "the positional move must complete close-enough");

    // Drain any moving-phase io_intr still buffered (the load-bearing step that
    // stops a leftover notification from handing the resume a free pass). This
    // runs synchronously, before the next await, so the forced poll the
    // completion just requested (write → StartPolling, serviced by the poll-loop
    // task only at the next await point) is NOT yet in the channel and is not
    // drained — see the current_thread requirement on the test. After this, the
    // only way a fresh io_intr arrives on the now-stationary, change-gated axis
    // is that forced poll.
    while io_intr_rx.try_recv().is_ok() {}

    // Phase 2: the held jog must resume. Without the finalize_motion forced
    // poll, no io_intr fires and the axis stays Idle until the deadline.
    let mut resumed = false;
    while Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(200), io_intr_rx.recv()).await {
            Ok(Some(())) => {
                setup.record.process().unwrap();
                setup.device_support.write(&mut setup.record).unwrap();
                if setup.record.stat.phase == MotionPhase::Jog {
                    resumed = true;
                    break;
                }
            }
            Ok(None) => break,
            Err(_) => continue,
        }
    }

    assert!(
        resumed,
        "held JOGF must resume via the real poll loop after a close-enough completion (phase={:?}, jogf={})",
        setup.record.stat.phase, setup.record.ctrl.jogf
    );

    let _ = setup.poll_cmd_tx.send(PollCommand::Shutdown).await;
    let _ = poll_handle.await;
}

// Helper: read the SimMotor's current dial position.
fn read_motor_pos(motor: &Arc<Mutex<dyn AsynMotor>>) -> f64 {
    let mut m = motor.lock().unwrap();
    m.poll(&asyn_rs::user::AsynUser::new(0)).unwrap().position
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

    let m = sim.lock().unwrap();
    assert_eq!(m.forwarded_high_limit, Some(100.0));
    assert_eq!(m.forwarded_low_limit, Some(-100.0));
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

    setup
        .record
        .put_field("DHLM", EpicsValue::Double(50.0))
        .unwrap();
    setup.record.process().unwrap();
    setup.device_support.write(&mut setup.record).unwrap();

    assert_eq!(sim.lock().unwrap().forwarded_high_limit, Some(50.0));
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

    // SimMotor's status does not report gain support; model a
    // gain-capable controller the way C reads it — from MSTA.
    setup.record.stat.msta.insert(MstaFlags::GAIN_SUPPORT);
    setup
        .record
        .put_field("PCOF", EpicsValue::Double(1.5))
        .unwrap();
    setup.record.process().unwrap();
    setup.device_support.write(&mut setup.record).unwrap();

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
        fn poll(&mut self, _user: &AsynUser) -> Result<MotorStatus, AsynError> {
            if self.offline.load(Ordering::SeqCst) {
                return Err(AsynError::Status {
                    status: AsynStatus::Timeout,
                    message: "controller not responding".into(),
                });
            }
            Ok(MotorStatus {
                position: self.position,
                done: true,
                ..MotorStatus::default()
            })
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
    let (io_intr_tx, mut io_intr_rx) = mpsc::channel::<()>(16);

    let poll_loop = motor_rs::poll_loop::MotorPollLoop::new(
        poll_cmd_rx,
        io_intr_tx,
        motor,
        device_state.clone(),
        Duration::from_millis(5),
        Duration::from_millis(5),
        0,
    );
    let poll_handle = tokio::spawn(poll_loop.run());

    // A healthy poll first, so the failure has a last-known status to carry
    // forward (C's parameter library keeps every field the failed poll never
    // wrote).
    poll_cmd_tx.send(PollCommand::StartPolling).await.unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(500), io_intr_rx.recv())
            .await
            .is_ok(),
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
        device_state
            .lock()
            .unwrap()
            .latest_status
            .as_ref()
            .is_some_and(|s| s.status.comms_error)
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

/// autosave pass0 restore contract. C `reboot_restore` pass0 writes fields
/// with `dbPut` BEFORE `init_record` runs (`initHookAfterInitDevSup`
/// precedes `initDatabase` in iocInit), so a restored VAL is a plain field
/// write: per-record device init clears the parked put (`clear_last_write`)
/// and the Startup readback's RSTM decision adopts the restored DVAL via
/// `SetPosition` (redefine). A restore must never dispatch a move — the
/// regression dispatched the restored VAL as a real move on the first
/// status pass, and on a fast axis the Startup readback then caught the
/// move mid-flight and synced the instantaneous position into VAL/DVAL.
#[tokio::test]
async fn pass0_restored_val_is_not_a_move_command() {
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
        fn poll(&mut self, _user: &AsynUser) -> Result<MotorStatus, AsynError> {
            Ok(MotorStatus {
                position: self.position,
                done: true,
                ..MotorStatus::default()
            })
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

    // pass0 restore: a silent field write, before device init.
    setup
        .record
        .put_field("VAL", EpicsValue::Double(1.5))
        .unwrap();
    // C init_record era: initial readback seed + parked-put clear.
    setup.device_support.init(&mut setup.record).unwrap();
    // First status pass = Startup -> initial_readback -> RSTM restore.
    setup.record.process().unwrap();
    setup.device_support.write(&mut setup.record).unwrap();

    assert_eq!(
        setup.record.pos.val, 1.5,
        "restored VAL survives init and the Startup pass"
    );
    assert_eq!(setup.record.pos.dval, 1.5);
    assert!(setup.record.stat.dmov, "restored axis comes up done");
    assert_eq!(
        moves.load(Ordering::SeqCst),
        0,
        "a pass0-restored VAL must never dispatch a move"
    );
    assert_eq!(
        loads.load(Ordering::SeqCst),
        1,
        "RSTM NearZero adopts the autosaved DVAL via SetPosition"
    );
    let user = AsynUser::new(0);
    let st = motor.lock().unwrap().poll(&user).unwrap();
    assert_eq!(st.position, 1.5, "the controller was redefined, not moved");
}
