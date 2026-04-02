//! C parity tests for backlash algorithm (2-phase: pretarget → final).

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
fn backlash_positive_bdst_negative_move() {
    // BDST=+1.0, moving negative → backlash needed
    let mut rec = make_record();
    rec.retry.bdst = 1.0;

    rec.put_field("VAL", EpicsValue::Double(-10.0)).unwrap();
    let effects = rec.plan_motion(CommandSource::Val);

    // Phase 1: pretarget = dval - bdst = -10 - 1 = -11
    assert_eq!(rec.stat.phase, MotionPhase::MainMove);
    assert!(rec.internal.backlash_pending);
    assert!(!rec.stat.dmov);
    assert!(rec.stat.mip.contains(MipFlags::MOVE));
    assert!(!rec.stat.tdir); // moving negative
    if let MotorCommand::MoveAbsolute {
        position, velocity, ..
    } = &effects.commands[0]
    {
        assert!((*position - (-11.0)).abs() < 1e-10);
        assert_eq!(*velocity, 10.0); // VELO for main move
    } else {
        panic!("expected MoveAbsolute");
    }

    // Complete phase 1
    complete_move(&mut rec, -11.0);
    let effects = rec.check_completion();

    // Phase 2: backlash final to dval=-10 with BVEL/BACC
    assert_eq!(rec.stat.phase, MotionPhase::BacklashFinal);
    assert!(rec.stat.mip.contains(MipFlags::MOVE_BL));
    assert!(!rec.internal.backlash_pending);
    assert!(!rec.stat.dmov);
    if let MotorCommand::MoveAbsolute {
        position,
        velocity,
        acceleration,
    } = &effects.commands[0]
    {
        assert!((*position - (-10.0)).abs() < 1e-10);
        assert_eq!(*velocity, 5.0); // BVEL
        assert_eq!(*acceleration, 0.5); // BACC
    } else {
        panic!("expected MoveAbsolute");
    }

    // Complete phase 2
    complete_move(&mut rec, -10.0);
    let _effects = rec.check_completion();

    assert!(rec.stat.dmov);
    assert_eq!(rec.stat.phase, MotionPhase::Idle);
    assert_eq!(rec.stat.mip, MipFlags::empty());
}

#[test]
fn backlash_negative_bdst_positive_move() {
    // BDST=-1.0, moving positive → backlash needed
    let mut rec = make_record();
    rec.retry.bdst = -1.0;

    rec.put_field("VAL", EpicsValue::Double(10.0)).unwrap();
    let effects = rec.plan_motion(CommandSource::Val);

    // Pretarget = dval - bdst = 10 - (-1) = 11
    assert!(rec.internal.backlash_pending);
    if let MotorCommand::MoveAbsolute { position, .. } = &effects.commands[0] {
        assert!((*position - 11.0).abs() < 1e-10);
    } else {
        panic!("expected MoveAbsolute");
    }

    complete_move(&mut rec, 11.0);
    let effects = rec.check_completion();

    // Final to dval=10
    assert_eq!(rec.stat.phase, MotionPhase::BacklashFinal);
    if let MotorCommand::MoveAbsolute { position, .. } = &effects.commands[0] {
        assert!((*position - 10.0).abs() < 1e-10);
    } else {
        panic!("expected MoveAbsolute");
    }

    complete_move(&mut rec, 10.0);
    let _effects = rec.check_completion();
    assert!(rec.stat.dmov);
}

#[test]
fn no_backlash_when_already_from_preferred_side() {
    // BDST=+1.0, moving positive → preferred direction, no backlash
    let mut rec = make_record();
    rec.retry.bdst = 1.0;

    rec.put_field("VAL", EpicsValue::Double(10.0)).unwrap();
    let effects = rec.plan_motion(CommandSource::Val);

    assert!(!rec.internal.backlash_pending);
    // Command goes directly to dval
    if let MotorCommand::MoveAbsolute { position, .. } = &effects.commands[0] {
        assert!((*position - 10.0).abs() < 1e-10);
    } else {
        panic!("expected MoveAbsolute");
    }

    complete_move(&mut rec, 10.0);
    let _effects = rec.check_completion();
    assert!(rec.stat.dmov);
}

#[test]
fn stop_during_backlash_main_move_cancels_final() {
    let mut rec = make_record();
    rec.retry.bdst = 1.0;

    rec.put_field("VAL", EpicsValue::Double(-10.0)).unwrap();
    rec.plan_motion(CommandSource::Val);

    assert!(rec.internal.backlash_pending);

    // Motor is moving to pretarget
    motor_moving(&mut rec, -5.0);

    // STOP during phase 1
    let effects = rec.plan_motion(CommandSource::Stop);

    assert!(rec.stat.mip.contains(MipFlags::STOP));
    assert!(!rec.internal.backlash_pending); // cleared
    assert!(matches!(effects.commands[0], MotorCommand::Stop { .. }));

    // Motor stops
    complete_move(&mut rec, -5.0);
    let _effects = rec.check_completion();
    assert!(rec.stat.dmov);
    assert_eq!(rec.stat.phase, MotionPhase::Idle);
}

#[test]
fn stop_during_backlash_final() {
    let mut rec = make_record();
    rec.retry.bdst = 1.0;

    rec.put_field("VAL", EpicsValue::Double(-10.0)).unwrap();
    rec.plan_motion(CommandSource::Val);

    // Complete main move to pretarget
    complete_move(&mut rec, -11.0);
    rec.check_completion();
    assert_eq!(rec.stat.phase, MotionPhase::BacklashFinal);

    // Motor is moving in backlash final
    motor_moving(&mut rec, -10.5);

    // STOP during backlash final
    let effects = rec.plan_motion(CommandSource::Stop);
    assert!(rec.stat.mip.contains(MipFlags::STOP));
    assert!(matches!(effects.commands[0], MotorCommand::Stop { .. }));

    // Motor stops
    complete_move(&mut rec, -10.5);
    let _effects = rec.check_completion();
    assert!(rec.stat.dmov);
    assert_eq!(rec.stat.phase, MotionPhase::Idle);
}

#[test]
fn backlash_then_retry_on_position_error() {
    let mut rec = make_record();
    rec.retry.bdst = 1.0;
    rec.retry.rdbd = 0.05;
    rec.retry.rtry = 3;
    rec.retry.rmod = RetryMode::Geometric;

    rec.put_field("VAL", EpicsValue::Double(-10.0)).unwrap();
    rec.plan_motion(CommandSource::Val);

    // Complete main move to pretarget
    complete_move(&mut rec, -11.0);
    rec.check_completion();

    // Complete backlash final with position error
    complete_move(&mut rec, -9.9); // error=0.1 > rdbd=0.05
    let effects = rec.check_completion();

    // Should enter retry
    assert_eq!(rec.stat.phase, MotionPhase::Retry);
    assert_eq!(rec.retry.rcnt, 1);
    assert!(rec.stat.mip.contains(MipFlags::RETRY));
    assert!(matches!(
        effects.commands[0],
        MotorCommand::MoveAbsolute { .. }
    ));
}

#[test]
fn no_backlash_when_bdst_zero() {
    let mut rec = make_record();
    rec.retry.bdst = 0.0;

    rec.put_field("VAL", EpicsValue::Double(-10.0)).unwrap();
    let effects = rec.plan_motion(CommandSource::Val);

    assert!(!rec.internal.backlash_pending);
    if let MotorCommand::MoveAbsolute { position, .. } = &effects.commands[0] {
        assert!((*position - (-10.0)).abs() < 1e-10);
    } else {
        panic!("expected MoveAbsolute");
    }
}
