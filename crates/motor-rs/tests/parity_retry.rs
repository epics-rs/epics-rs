//! C parity tests for retry/RMOD/FRAC/SPDB semantics.

use motor_rs::flags::*;
use motor_rs::record::MotorRecord;

use asyn_rs::interfaces::motor::MotorStatus;

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

#[test]
fn rmod_default_retry_uses_rdbd_step() {
    let mut rec = make_record();
    rec.retry.rdbd = 0.1;
    rec.retry.rtry = 5;
    rec.retry.rmod = RetryMode::Default;

    rec.pos.dval = 10.0;
    rec.plan_motion(CommandSource::Val);

    // Motor stops short by 1.0
    complete_move(&mut rec, 9.0);
    let effects = rec.check_completion();

    assert_eq!(rec.stat.phase, MotionPhase::Retry);
    assert_eq!(rec.retry.rcnt, 1);
    // Default: target = drbv + rdbd * sign(error)
    // = 9.0 + 0.1 * 1.0 = 9.1
    if let MotorCommand::MoveAbsolute { position, .. } = &effects.commands[0] {
        assert!((*position - 9.1).abs() < 1e-10);
    } else {
        panic!("expected MoveAbsolute");
    }
}

#[test]
fn rmod_arithmetic_retry() {
    let mut rec = make_record();
    rec.retry.rdbd = 0.01;
    rec.retry.rtry = 5;
    rec.retry.frac = 0.5;
    rec.retry.rmod = RetryMode::Arithmetic;

    rec.pos.dval = 10.0;
    rec.plan_motion(CommandSource::Val);

    complete_move(&mut rec, 9.0);
    let effects = rec.check_completion();

    assert_eq!(rec.stat.phase, MotionPhase::Retry);
    // Arithmetic: target = drbv + (dval - drbv) * frac = 9.0 + 1.0*0.5 = 9.5
    if let MotorCommand::MoveAbsolute { position, .. } = &effects.commands[0] {
        assert!((*position - 9.5).abs() < 1e-10);
    } else {
        panic!("expected MoveAbsolute");
    }
}

#[test]
fn rmod_geometric_retry() {
    let mut rec = make_record();
    rec.retry.rdbd = 0.01;
    rec.retry.rtry = 5;
    rec.retry.rmod = RetryMode::Geometric;

    rec.pos.dval = 10.0;
    rec.plan_motion(CommandSource::Val);

    complete_move(&mut rec, 9.0);
    let effects = rec.check_completion();

    assert_eq!(rec.stat.phase, MotionPhase::Retry);
    // Geometric: target = dval
    if let MotorCommand::MoveAbsolute { position, .. } = &effects.commands[0] {
        assert!((*position - 10.0).abs() < 1e-10);
    } else {
        panic!("expected MoveAbsolute");
    }
}

#[test]
fn rmod_inposition_no_retry() {
    let mut rec = make_record();
    rec.retry.rdbd = 0.1;
    rec.retry.rtry = 5;
    rec.retry.rmod = RetryMode::InPosition;

    rec.pos.dval = 10.0;
    rec.plan_motion(CommandSource::Val);

    // Motor stops with error > rdbd
    complete_move(&mut rec, 9.0);
    let effects = rec.check_completion();

    // InPosition mode: no retry, just finalize
    assert!(rec.stat.dmov);
    assert_eq!(rec.stat.phase, MotionPhase::Idle);
    assert!(effects.commands.is_empty());
    assert_eq!(rec.retry.rcnt, 0); // never incremented
}

#[test]
fn rcnt_increments_and_resets() {
    let mut rec = make_record();
    rec.retry.rdbd = 0.1;
    rec.retry.rtry = 3;
    rec.retry.rmod = RetryMode::Geometric;

    rec.pos.dval = 10.0;
    rec.plan_motion(CommandSource::Val);

    // Retry 1
    complete_move(&mut rec, 9.0);
    rec.check_completion();
    assert_eq!(rec.retry.rcnt, 1);

    // Retry 2
    complete_move(&mut rec, 9.5);
    rec.check_completion();
    assert_eq!(rec.retry.rcnt, 2);

    // Retry 3
    complete_move(&mut rec, 9.8);
    rec.check_completion();
    assert_eq!(rec.retry.rcnt, 3);

    // Exhausted — MISS set, finalize
    complete_move(&mut rec, 9.85); // still > rdbd
    let _effects = rec.check_completion();
    assert!(rec.retry.miss);
    assert!(rec.stat.dmov);

    // New move resets RCNT
    rec.pos.dval = 20.0;
    rec.plan_motion(CommandSource::Val);
    assert_eq!(rec.retry.rcnt, 0);
    assert!(!rec.retry.miss);
}

#[test]
fn spdb_suppresses_small_move() {
    let mut rec = make_record();
    rec.retry.spdb = 0.5;
    rec.pos.drbv = 10.0;

    // Move within SPDB deadband
    rec.pos.dval = 10.3; // |10.3 - 10.0| = 0.3 <= 0.5
    let effects = rec.plan_motion(CommandSource::Val);

    // No move initiated
    assert!(effects.commands.is_empty());
    assert!(rec.stat.dmov); // unchanged
}

#[test]
fn spdb_allows_larger_move() {
    let mut rec = make_record();
    rec.retry.spdb = 0.5;
    rec.pos.drbv = 10.0;

    // Move outside SPDB deadband
    rec.pos.dval = 11.0; // |11.0 - 10.0| = 1.0 > 0.5
    let effects = rec.plan_motion(CommandSource::Val);

    assert!(!effects.commands.is_empty());
    assert!(!rec.stat.dmov);
}

#[test]
fn spdb_vs_rdbd_are_independent() {
    let mut rec = make_record();
    rec.retry.spdb = 0.5; // move initiation deadband
    rec.retry.rdbd = 0.01; // completion deadband
    rec.retry.rtry = 3;
    rec.retry.rmod = RetryMode::Geometric;

    // Move large enough to pass SPDB
    rec.pos.dval = 10.0;
    rec.plan_motion(CommandSource::Val);

    // Motor stops with error > RDBD but < SPDB
    complete_move(&mut rec, 9.8); // error=0.2 > rdbd=0.01
    let effects = rec.check_completion();

    // Should still retry (RDBD controls retry, not SPDB)
    assert_eq!(rec.stat.phase, MotionPhase::Retry);
    assert_eq!(rec.retry.rcnt, 1);
    assert!(!effects.commands.is_empty());
}
