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

#[test]
fn jog_bl2_relative_leg_is_frac_times_bdst() {
    // C MIP_JOG_BL1 branch (motorRecord.cc:1009): the relative final leg
    // is (relpos - relbpos) * frac == BDST * frac — a fixed stroke,
    // independent of where the takeout leg actually stopped. The
    // remaining-error form (dval - drbv) * frac belongs to MOVE_BL (957).
    let mut rec = make_record();
    rec.retry.bdst = 2.0;
    rec.retry.rtry = 5; // use_rel needs RTRY != 0 ...
    rec.conv.ueip = true; // ... and the encoder in use
    rec.conv.eres = rec.conv.mres;
    rec.retry.frac = 0.5;

    rec.ctrl.jogf = true;
    rec.plan_motion(CommandSource::Jogf);
    rec.stat.msta = MstaFlags::MOVING;
    rec.stat.movn = true;
    rec.ctrl.jogf = false;
    rec.plan_motion(CommandSource::Jogf); // commanded release -> JogStopping

    // Stop lands at 30.0: BL1 takeout leg goes bdst back (-2.0).
    complete_move(&mut rec, 30.0);
    let effects = rec.check_completion();
    assert_eq!(rec.stat.phase, MotionPhase::JogBacklash);
    let MotorCommand::MoveRelative { distance, .. } = &effects.commands[0] else {
        panic!("expected relative BL1 leg, got {:?}", effects.commands);
    };
    assert!((*distance - (-2.0)).abs() < 1e-9, "BL1 leg, got {distance}");

    // BL1 stops short of the pretarget (28.0) at 27.9. C still strokes
    // exactly frac * bdst = 1.0; the old remaining-error form would
    // emit (30 - 27.9) * 0.5 = 1.05.
    complete_move(&mut rec, 27.9);
    let effects = rec.check_completion();
    assert!(rec.stat.mip.contains(MipFlags::JOG_BL2));
    let MotorCommand::MoveRelative { distance, .. } = &effects.commands[0] else {
        panic!("expected relative BL2 leg, got {:?}", effects.commands);
    };
    assert!(
        (*distance - 1.0).abs() < 1e-9,
        "BL2 = frac*bdst, got {distance}"
    );
}

#[test]
fn backlash_final_leg_posts_cdir_and_frac_scaled_rval() {
    // C MOVE_BL arm (motorRecord.cc:967, 973): the absolute final leg
    // posts RVAL = NINT of the FRAC-scaled commanded raw position and
    // re-derives CDIR from the raw error sign.
    let mut rec = make_record();
    rec.retry.bdst = 1.0;
    rec.retry.frac = 0.5;

    // bdst > 0, moving negative: two legs through pretarget -11.
    rec.put_field("VAL", EpicsValue::Double(-10.0)).unwrap();
    rec.plan_motion(CommandSource::Val);
    assert!(rec.internal.backlash_pending);
    assert!(!rec.stat.cdir, "first leg runs negative");

    // First leg lands at the pretarget; final leg goes forward to
    // pretarget + frac*bdst = -10.5.
    complete_move(&mut rec, -11.0);
    let effects = rec.check_completion();
    assert!(rec.stat.mip.contains(MipFlags::MOVE_BL));
    let MotorCommand::MoveAbsolute { position, .. } = &effects.commands[0] else {
        panic!("expected absolute final leg, got {:?}", effects.commands);
    };
    assert!((*position - (-10.5)).abs() < 1e-9, "got {position}");
    assert!(rec.stat.cdir, "final leg runs positive (C 973)");
    assert_eq!(
        rec.pos.rval, -10500,
        "RVAL posts the FRAC-scaled raw target (C 967), not DVAL/MRES"
    );
}

#[test]
fn jog_bl2_posts_cdir_from_scaled_stroke() {
    // C MIP_JOG_BL1 branch (motorRecord.cc:1020): CDIR follows the
    // sign of the (frac-scaled) relative stroke = BDST direction.
    let mut rec = make_record();
    rec.retry.bdst = -2.0; // negative backlash: final leg runs negative
    rec.retry.rtry = 5;
    rec.conv.ueip = true;
    rec.conv.eres = rec.conv.mres;
    rec.retry.frac = 1.0;

    rec.ctrl.jogf = true;
    rec.plan_motion(CommandSource::Jogf);
    rec.stat.msta = MstaFlags::MOVING;
    rec.stat.movn = true;
    rec.ctrl.jogf = false;
    rec.plan_motion(CommandSource::Jogf);

    // Stop at 30: takeout leg goes to 32 (dval - bdst), positive.
    complete_move(&mut rec, 30.0);
    rec.check_completion();
    assert!(rec.stat.mip.contains(MipFlags::JOG_BL1));

    // Takeout done; BL2 strokes bdst*frac = -2.0, so CDIR flips negative.
    complete_move(&mut rec, 32.0);
    rec.check_completion();
    assert!(rec.stat.mip.contains(MipFlags::JOG_BL2));
    assert!(!rec.stat.cdir, "BL2 direction follows BDST sign (C 1020)");
}

#[test]
fn backlash_final_leg_velocity_floored_above_vbas() {
    // C MOVE_BL arm (motorRecord.cc:953-954): a backlash velocity that
    // does not exceed VBAS is floored to one raw step/s above it —
    // vbas + |mres| in EGU. C compares with <=, so equality clamps too.
    let mut rec = make_record();
    rec.retry.bdst = 1.0;
    rec.vel.vbas = rec.vel.bvel; // bvel == vbas: clamp fires

    rec.put_field("VAL", EpicsValue::Double(-10.0)).unwrap();
    rec.plan_motion(CommandSource::Val);
    complete_move(&mut rec, -11.0);
    let effects = rec.check_completion();
    assert!(rec.stat.mip.contains(MipFlags::MOVE_BL));
    let MotorCommand::MoveAbsolute { velocity, .. } = &effects.commands[0] else {
        panic!("expected absolute final leg, got {:?}", effects.commands);
    };
    assert!(
        (*velocity - (rec.vel.vbas + 0.001)).abs() < 1e-9,
        "bvel <= vbas floors to vbas + |mres|, got {velocity}"
    );
}

#[test]
fn jog_takeout_leg_velocity_floored_above_vbas() {
    // C MIP_JOG_STOP arm (motorRecord.cc:936-937): same floor for the
    // takeout leg, which runs at VELO.
    let mut rec = make_record();
    rec.retry.bdst = 2.0;
    rec.vel.vbas = rec.vel.velo; // velo == vbas: clamp fires

    rec.ctrl.jogf = true;
    rec.plan_motion(CommandSource::Jogf);
    rec.stat.msta = MstaFlags::MOVING;
    rec.stat.movn = true;
    rec.ctrl.jogf = false;
    rec.plan_motion(CommandSource::Jogf);

    complete_move(&mut rec, 30.0);
    let effects = rec.check_completion();
    assert!(rec.stat.mip.contains(MipFlags::JOG_BL1));
    let MotorCommand::MoveAbsolute { velocity, .. } = &effects.commands[0] else {
        panic!("expected absolute takeout leg, got {:?}", effects.commands);
    };
    assert!(
        (*velocity - (rec.vel.vbas + 0.001)).abs() < 1e-9,
        "velo <= vbas floors to vbas + |mres|, got {velocity}"
    );
}
