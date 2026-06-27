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

/// C init_record 716-718 ("Reset limits in case database values are
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
