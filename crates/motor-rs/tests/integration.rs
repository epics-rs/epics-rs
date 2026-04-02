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
