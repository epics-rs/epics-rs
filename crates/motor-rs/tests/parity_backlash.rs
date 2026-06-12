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
        // EGU/sec²: (BVEL - VBAS) / BACC = (5.0 - 0.0) / 0.5
        assert_eq!(*acceleration, 10.0);
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
fn negative_bdst_relative_move_preferred_direction() {
    // C: 524696a8 PR #182 / issue #181 — negative BDST with RTRY>0 and a
    // relative-move axis (UEIP). The negative-BDST within-range arm
    // (C 2493: bdst < 0 && relbpos >= 1.0) selects the single-leg
    // backlash-speed dispatch: a preferred move already within one
    // backlash of the target goes straight there at BVEL.
    let mut rec = make_record();
    rec.retry.bdst = -1.0;
    rec.retry.rtry = 3;
    rec.conv.ueip = true; // → use_relative_moves() == true
    rec.pos.drbv = -9.5; // relbpos = (-9 - (-9.5))/mres = +500 steps >= 1

    rec.put_field("VAL", EpicsValue::Double(-10.0)).unwrap();
    let effects = rec.plan_motion(CommandSource::Val);

    // Preferred + within range: single leg, no pretarget.
    assert!(!rec.internal.backlash_pending);
    let leg = effects.commands.iter().find_map(|c| match c {
        MotorCommand::MoveRelative {
            distance, velocity, ..
        } => Some((*distance, *velocity)),
        _ => None,
    });
    let (dist, vel) = leg.expect("MoveRelative expected");
    assert!(
        (dist - (-0.5)).abs() < 1e-9,
        "remaining error leg, got {dist}"
    );
    assert_eq!(vel, 5.0, "within-range leg runs at BVEL");
}

#[test]
fn negative_bdst_relative_move_outside_range_two_legs() {
    // Same axis far from the target: relbpos = (-9 - 0)/0.001 = -9000
    // steps fails the bdst<0 within-range arm (C 2493), and BVEL != VELO
    // keeps it out of case 1 — the move takes the two-leg correction
    // through the pretarget (C 2518-2524) even though the direction is
    // preferred.
    let mut rec = make_record();
    rec.retry.bdst = -1.0;
    rec.retry.rtry = 3;
    rec.conv.ueip = true;
    rec.pos.drbv = 0.0;

    rec.put_field("VAL", EpicsValue::Double(-10.0)).unwrap();
    let effects = rec.plan_motion(CommandSource::Val);

    assert!(rec.internal.backlash_pending);
    let dist = effects.commands.iter().find_map(|c| match c {
        MotorCommand::MoveRelative { distance, .. } => Some(*distance),
        _ => None,
    });
    assert_eq!(dist, Some(-9.0), "first leg to pretarget dval - bdst = -9");
}

#[test]
fn negative_bdst_relative_move_non_preferred_uses_backlash() {
    // Negative BDST, positive (non-preferred) move on a relative-move axis:
    // backlash pretarget = dval - bdst = 10 - (-1) = 11, then final to 10.
    let mut rec = make_record();
    rec.retry.bdst = -1.0;
    rec.retry.rtry = 3;
    rec.conv.ueip = true;
    rec.pos.drbv = 0.0;

    rec.put_field("VAL", EpicsValue::Double(10.0)).unwrap();
    let effects = rec.plan_motion(CommandSource::Val);

    assert!(rec.internal.backlash_pending);
    // Phase 1 relative distance to pretarget 11 (from drbv 0)
    let dist = effects.commands.iter().find_map(|c| match c {
        MotorCommand::MoveRelative { distance, .. } => Some(*distance),
        _ => None,
    });
    assert_eq!(dist, Some(11.0));

    complete_move(&mut rec, 11.0);
    let effects = rec.check_completion();
    assert_eq!(rec.stat.phase, MotionPhase::BacklashFinal);
    // Final approach to dval=10
    let pos = effects.commands.iter().find_map(|c| match c {
        MotorCommand::MoveRelative { distance, .. } => Some(11.0 + *distance),
        MotorCommand::MoveAbsolute { position, .. } => Some(*position),
        _ => None,
    });
    assert!((pos.unwrap() - 10.0).abs() < 1e-9);
}

#[test]
fn no_backlash_when_already_from_preferred_side() {
    // BDST=+1.0, moving positive (preferred) with BVEL == VELO and
    // BACC == ACCL: C case 1 (2479-2489) — one leg straight to dval.
    let mut rec = make_record();
    rec.retry.bdst = 1.0;
    rec.vel.bvel = rec.vel.velo;
    rec.vel.bacc = rec.vel.accl;

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
fn preferred_direction_bvel_differs_takes_two_legs() {
    // BDST=+1.0, moving positive (preferred) but BVEL != VELO and the
    // target is outside the backlash range: C falls to the else case
    // (2518-2524) — first leg to the pretarget at slew speed, final
    // BDST stretch at backlash speed from postprocess.
    let mut rec = make_record();
    rec.retry.bdst = 1.0;

    rec.put_field("VAL", EpicsValue::Double(10.0)).unwrap();
    let effects = rec.plan_motion(CommandSource::Val);

    assert!(rec.internal.backlash_pending);
    if let MotorCommand::MoveAbsolute {
        position, velocity, ..
    } = &effects.commands[0]
    {
        assert!(
            (*position - 9.0).abs() < 1e-10,
            "first leg to pretarget dval - bdst = 9, got {position}"
        );
        assert_eq!(*velocity, 10.0, "first leg at VELO");
    } else {
        panic!("expected MoveAbsolute");
    }

    complete_move(&mut rec, 9.0);
    let effects = rec.check_completion();
    assert_eq!(rec.stat.phase, MotionPhase::BacklashFinal);
    if let MotorCommand::MoveAbsolute {
        position, velocity, ..
    } = &effects.commands[0]
    {
        assert!((*position - 10.0).abs() < 1e-10);
        assert_eq!(*velocity, 5.0, "final leg at BVEL");
    } else {
        panic!("expected MoveAbsolute");
    }

    complete_move(&mut rec, 10.0);
    rec.check_completion();
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

    // Retry replays the full move block (C: maybeRetry arms MIP_RETRY,
    // do_work re-dispatches with mip |= MIP_MOVE at 2461): the retry of
    // dval == ldvl with BDST > 0 is non-preferred (C 2391-2395), so it
    // re-runs the backlash sequence starting at the pretarget.
    assert_eq!(rec.stat.phase, MotionPhase::MainMove);
    assert!(
        rec.stat.mip.contains(MipFlags::RETRY),
        "retry marked in MIP"
    );
    assert!(rec.stat.mip.contains(MipFlags::MOVE));
    assert_eq!(rec.retry.rcnt, 1);
    assert!(rec.internal.backlash_pending);
    if let MotorCommand::MoveAbsolute { position, .. } = &effects.commands[0] {
        assert!(
            (*position - (-11.0)).abs() < 1e-10,
            "retry with backlash dispatches to the pretarget, got {position}"
        );
    } else {
        panic!("expected MoveAbsolute, got {:?}", effects.commands);
    }
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

#[test]
fn frac_dispatch_walks_from_previous_target_not_readback() {
    // C 2268/2488: the absolute dispatch is
    // position = currpos + frac*(newpos - currpos) with currpos = LDVL —
    // the previously dispatched target — not DRBV. After an undershot
    // move finalizes (ldvl=10, drbv=9.4), a new FRAC=0.5 move to 20 must
    // dispatch 10 + 0.5*(20-10) = 15.0, not 9.4 + 0.5*10.6 = 14.7.
    let mut rec = make_record();
    rec.retry.frac = 0.5;
    rec.retry.rtry = 0; // finalize on first completion

    rec.put_field("VAL", EpicsValue::Double(10.0)).unwrap();
    rec.plan_motion(CommandSource::Val);
    complete_move(&mut rec, 9.4); // undershoot; rtry=0 finalizes
    rec.check_completion();
    assert!(rec.stat.dmov);
    assert_eq!(rec.internal.ldvl, 10.0);

    rec.put_field("VAL", EpicsValue::Double(20.0)).unwrap();
    let effects = rec.plan_motion(CommandSource::Val);
    if let MotorCommand::MoveAbsolute { position, .. } = &effects.commands[0] {
        assert!(
            (*position - 15.0).abs() < 1e-9,
            "FRAC walks from LDVL (expected 15.0), got {position}"
        );
    } else {
        panic!("expected MoveAbsolute, got {:?}", effects.commands);
    }
}
