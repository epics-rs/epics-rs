//! C parity tests for NTM (New Target Monitor) process path connection.

use motor_rs::flags::*;
use motor_rs::record::MotorRecord;

use asyn_rs::interfaces::motor::MotorStatus;
use epics_base_rs::server::record::Record;
use epics_base_rs::types::EpicsValue;

fn make_record() -> MotorRecord {
    let mut rec = MotorRecord::new();
    rec.conv.mres = 0.001;
    rec.limits.dhlm = 100.0;
    rec.limits.dllm = -100.0;
    rec.limits.hlm = 100.0;
    rec.limits.llm = -100.0;
    rec.limits.lvio = false;
    rec.vel.velo = 10.0;
    rec.vel.accl = 0.5;
    rec.vel.bvel = 5.0;
    rec.vel.bacc = 0.5;
    rec.stat.msta = MstaFlags::DONE;
    rec.timing.ntm = true;
    rec.timing.ntmf = 2.0;
    rec
}

fn complete_move(rec: &mut MotorRecord, target_pos: f64) {
    let status = MotorStatus {
        position: target_pos,
        encoder_position: target_pos,
        done: true,
        moving: false,
        ..Default::default()
    };
    rec.process_motor_info(&status);
}

fn motor_moving(rec: &mut MotorRecord, current_pos: f64) {
    let status = MotorStatus {
        position: current_pos,
        encoder_position: current_pos,
        done: false,
        moving: true,
        ..Default::default()
    };
    rec.process_motor_info(&status);
}

#[test]
fn val_during_motion_triggers_retarget() {
    let mut rec = make_record();

    // Start move to 50
    rec.put_field("VAL", EpicsValue::Double(50.0)).unwrap();
    rec.plan_motion(CommandSource::Val);
    rec.internal.ldvl = 50.0;
    assert!(!rec.stat.dmov);
    assert_eq!(rec.stat.phase, MotionPhase::MainMove);

    // Motor moving at 25
    motor_moving(&mut rec, 25.0);

    // New target: 80 (same direction, farther) → ExtendMove
    rec.put_field("VAL", EpicsValue::Double(80.0)).unwrap();
    let effects = rec.plan_motion(CommandSource::Val);

    // Should issue new MoveAbsolute to 80
    assert_eq!(effects.commands.len(), 1);
    if let MotorCommand::MoveAbsolute { position, .. } = &effects.commands[0] {
        assert!((*position - 80.0).abs() < 1e-10);
    } else {
        panic!("expected MoveAbsolute");
    }
    assert!(!rec.stat.dmov);
}

#[test]
fn opposite_direction_retarget_stops_and_replans() {
    let mut rec = make_record();

    // Start move to 50
    rec.put_field("VAL", EpicsValue::Double(50.0)).unwrap();
    rec.plan_motion(CommandSource::Val);
    rec.internal.ldvl = 50.0;

    motor_moving(&mut rec, 25.0);

    // New target: -20 (opposite direction) → StopAndReplan
    rec.put_field("VAL", EpicsValue::Double(-20.0)).unwrap();
    let effects = rec.plan_motion(CommandSource::Val);

    // Should issue Stop and store pending retarget
    assert_eq!(effects.commands.len(), 1);
    assert!(matches!(effects.commands[0], MotorCommand::Stop { .. }));
    assert!(rec.stat.mip.contains(MipFlags::STOP));
    assert_eq!(rec.internal.pending_retarget, Some(-20.0));

    // Motor stops
    complete_move(&mut rec, 25.0);
    let effects = rec.check_completion();

    // Should replan to -20
    assert!(!rec.stat.dmov);
    assert_eq!(rec.stat.phase, MotionPhase::MainMove);
    assert_eq!(effects.commands.len(), 1);
    if let MotorCommand::MoveAbsolute { position, .. } = &effects.commands[0] {
        assert!((*position - (-20.0)).abs() < 1e-10);
    } else {
        panic!("expected MoveAbsolute for replan");
    }
}

#[test]
fn ntm_false_ignores_retarget() {
    let mut rec = make_record();
    rec.timing.ntm = false;

    rec.put_field("VAL", EpicsValue::Double(50.0)).unwrap();
    rec.plan_motion(CommandSource::Val);
    rec.internal.ldvl = 50.0;

    motor_moving(&mut rec, 25.0);

    // New target while NTM=false → ignored
    rec.put_field("VAL", EpicsValue::Double(80.0)).unwrap();
    let effects = rec.plan_motion(CommandSource::Val);

    // Should be ignored (no commands)
    assert!(effects.commands.is_empty());
}

#[test]
fn retarget_during_backlash_cancels_pending() {
    let mut rec = make_record();
    rec.retry.bdst = 1.0;

    // Start move with backlash
    rec.put_field("VAL", EpicsValue::Double(-10.0)).unwrap();
    rec.plan_motion(CommandSource::Val);
    rec.internal.ldvl = -10.0;
    assert!(rec.internal.backlash_pending);

    motor_moving(&mut rec, -5.0);

    // Retarget while backlash is pending
    rec.put_field("VAL", EpicsValue::Double(20.0)).unwrap();
    let effects = rec.plan_motion(CommandSource::Val);

    // Should clear backlash_pending
    assert!(!rec.internal.backlash_pending);
    // Either stop-and-replan or extend
    assert!(!effects.commands.is_empty());
}

#[test]
fn retarget_during_retry_resets_rcnt() {
    let mut rec = make_record();
    rec.retry.rdbd = 0.1;
    rec.retry.rtry = 5;
    rec.retry.rmod = RetryMode::Geometric;

    rec.put_field("VAL", EpicsValue::Double(10.0)).unwrap();
    rec.plan_motion(CommandSource::Val);
    rec.internal.ldvl = 10.0;

    // Enter retry
    complete_move(&mut rec, 9.5);
    rec.check_completion();
    assert_eq!(rec.stat.phase, MotionPhase::Retry);
    assert_eq!(rec.retry.rcnt, 1);

    // Motor retrying
    motor_moving(&mut rec, 9.7);

    // Retarget during retry
    rec.put_field("VAL", EpicsValue::Double(20.0)).unwrap();
    let effects = rec.plan_motion(CommandSource::Val);

    // RCNT should be reset
    assert_eq!(rec.retry.rcnt, 0);
    assert!(!effects.commands.is_empty());
}

#[test]
fn stop_and_replan_with_backlash() {
    let mut rec = make_record();
    rec.retry.bdst = 1.0;

    // Start move to 50
    rec.put_field("VAL", EpicsValue::Double(50.0)).unwrap();
    rec.plan_motion(CommandSource::Val);
    rec.internal.ldvl = 50.0;

    motor_moving(&mut rec, 25.0);

    // Retarget to -10 (opposite direction, needs backlash)
    rec.put_field("VAL", EpicsValue::Double(-10.0)).unwrap();
    let effects = rec.plan_motion(CommandSource::Val);

    // Should stop
    assert!(matches!(effects.commands[0], MotorCommand::Stop { .. }));
    assert_eq!(rec.internal.pending_retarget, Some(-10.0));

    // Motor stops, replan happens
    complete_move(&mut rec, 25.0);
    let effects = rec.check_completion();

    // Replan should include backlash (moving negative with BDST=+1)
    assert_eq!(rec.stat.phase, MotionPhase::MainMove);
    assert!(rec.internal.backlash_pending);
    if let MotorCommand::MoveAbsolute { position, .. } = &effects.commands[0] {
        // Pretarget = -10 - 1 = -11
        assert!((*position - (-11.0)).abs() < 1e-10);
    } else {
        panic!("expected MoveAbsolute");
    }
}

#[test]
fn spmg_go_resume_from_pause() {
    let mut rec = make_record();

    // Start move
    rec.put_field("VAL", EpicsValue::Double(50.0)).unwrap();
    rec.plan_motion(CommandSource::Val);
    motor_moving(&mut rec, 25.0);

    // Pause
    rec.ctrl.spmg = SpmgMode::Pause;
    rec.plan_motion(CommandSource::Spmg);
    assert!(rec.stat.dmov);
    assert_eq!(rec.pos.dval, 50.0); // target preserved

    // Resume with Go
    rec.ctrl.spmg = SpmgMode::Go;
    let effects = rec.plan_motion(CommandSource::Spmg);

    // Should replan to saved target
    assert!(!rec.stat.dmov);
    assert_eq!(rec.stat.phase, MotionPhase::MainMove);
    assert!(!effects.commands.is_empty());
}
