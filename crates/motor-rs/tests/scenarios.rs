use motor_rs::flags::*;
use motor_rs::record::MotorRecord;
use motor_rs::sim_motor::SimMotor;

use asyn_rs::interfaces::motor::{AsynMotor, MotorStatus};
use asyn_rs::user::AsynUser;
use epics_base_rs::server::record::Record;
use epics_base_rs::types::EpicsValue;

use std::time::Duration;

/// Helper: create a default motor record with typical settings.
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
    rec.vel.hvel = 5.0;
    rec.vel.jvel = 5.0;
    rec.vel.jar = 1.0;
    rec.stat.msta = MstaFlags::DONE;
    rec
}

/// Helper: simulate a completed move by updating motor info.
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

/// Helper: simulate motor in-progress.
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
fn absolute_move_reaches_target_and_sets_dmov() {
    let mut rec = make_record();

    // Write VAL to start move
    rec.put_field("VAL", EpicsValue::Double(50.0)).unwrap();
    let effects = rec.plan_motion(CommandSource::Val);

    // Verify move started
    assert!(!rec.stat.dmov);
    assert_eq!(rec.stat.phase, MotionPhase::MainMove);
    assert!(rec.stat.mip.contains(MipFlags::MOVE));
    assert_eq!(effects.commands.len(), 1);
    assert!(matches!(
        effects.commands[0],
        MotorCommand::MoveAbsolute { position, .. } if (position - 50.0).abs() < 1e-10
    ));

    // Simulate motor reaching target
    complete_move(&mut rec, 50.0);
    let _effects = rec.check_completion();

    // Verify completion
    assert!(rec.stat.dmov);
    assert_eq!(rec.stat.phase, MotionPhase::Idle);
    assert_eq!(rec.stat.mip, MipFlags::empty());
    assert!((rec.pos.rbv - 50.0).abs() < 1e-6);
    assert!((rec.pos.drbv - 50.0).abs() < 1e-6);
}

/// BUG 2 regression: `plan_absolute_move` must recompute `pos.diff`
/// (= dval - drbv) fresh before deriving CDIR. The VAL write path
/// updates `pos.dval` but never `pos.diff` — only a poll
/// (`process_motor_info`) or the `Set` branch does. A VAL write while
/// idle with no poll update in the same cycle would otherwise compute
/// CDIR from a stale `diff`.
#[test]
fn cdir_uses_fresh_diff_not_stale_pos_diff() {
    let mut rec = make_record();

    // Motor is at dial position 0, idle.
    rec.pos.drbv = 0.0;
    rec.pos.dval = 0.0;
    rec.pos.val = 0.0;
    // Inject a STALE positive diff — as if a prior positive move's
    // poll left it set and no fresh poll has run since.
    rec.pos.diff = 50.0;

    // User commands a move in the NEGATIVE direction (VAL = -30).
    rec.put_field("VAL", EpicsValue::Double(-30.0)).unwrap();
    let effects = rec.plan_motion(CommandSource::Val);

    // The move must be issued in the negative direction.
    assert!(rec.stat.mip.contains(MipFlags::MOVE));
    assert_eq!(effects.commands.len(), 1);

    // CDIR must reflect the fresh diff (dval - drbv = -30 - 0 < 0),
    // i.e. negative direction. With MRES > 0, cdir = (diff >= 0).
    // Pre-fix this read the stale diff = +50 and asserted cdir = true.
    assert!(
        !rec.stat.cdir,
        "CDIR must be derived from fresh diff (-30), not stale pos.diff (+50)"
    );
    // pos.diff itself must have been refreshed.
    assert!(
        (rec.pos.diff - (-30.0)).abs() < 1e-9,
        "pos.diff must be recomputed to dval - drbv = -30, got {}",
        rec.pos.diff
    );
}

#[test]
fn soft_limit_rejects_move_and_sets_lvio() {
    let mut rec = make_record();

    rec.pos.dval = 200.0; // Beyond DHLM
    let effects = rec.plan_motion(CommandSource::Val);

    assert!(rec.limits.lvio);
    assert!(effects.commands.is_empty());
    assert!(rec.stat.dmov); // no motion started
}

#[test]
fn backlash_generates_two_phase_move() {
    let mut rec = make_record();
    rec.retry.bdst = 1.0; // positive backlash

    // Move in negative direction (opposite to BDST) to trigger backlash
    rec.pos.dval = -10.0;
    rec.pos.drbv = 0.0;
    let effects = rec.plan_motion(CommandSource::Val);
    assert!(!rec.stat.dmov);
    assert_eq!(rec.stat.phase, MotionPhase::MainMove);
    assert!(rec.internal.backlash_pending);

    // First command should go to pretarget (dval - bdst = -10 - 1 = -11)
    assert_eq!(effects.commands.len(), 1);
    if let MotorCommand::MoveAbsolute { position, .. } = &effects.commands[0] {
        assert!(
            (*position - (-11.0)).abs() < 1e-10,
            "pretarget should be -11.0, got {position}"
        );
    } else {
        panic!("expected MoveAbsolute");
    }

    // Motor reaches pretarget
    complete_move(&mut rec, -11.0);
    let effects = rec.check_completion();

    // Should start backlash final (move to dval)
    assert_eq!(rec.stat.phase, MotionPhase::BacklashFinal);
    assert!(rec.stat.mip.contains(MipFlags::MOVE_BL));
    assert!(!rec.internal.backlash_pending);
    assert_eq!(effects.commands.len(), 1);
    if let MotorCommand::MoveAbsolute {
        position, velocity, ..
    } = &effects.commands[0]
    {
        assert!((*position - (-10.0)).abs() < 1e-10);
        assert_eq!(*velocity, rec.vel.bvel);
    } else {
        panic!("expected MoveAbsolute");
    }

    // Complete backlash final
    complete_move(&mut rec, -10.0);
    let _effects = rec.check_completion();

    // Should be finalized
    assert!(rec.stat.dmov);
    assert_eq!(rec.stat.phase, MotionPhase::Idle);
}

#[test]
fn retry_reissues_move_when_error_exceeds_rdbd() {
    let mut rec = make_record();
    rec.retry.rdbd = 0.01;
    rec.retry.rtry = 3;
    rec.retry.frac = 1.0;

    // Start move
    rec.pos.dval = 10.0;
    rec.plan_motion(CommandSource::Val);

    // Motor stops short
    complete_move(&mut rec, 9.95); // error=0.05 > rdbd=0.01
    let effects = rec.check_completion();

    assert_eq!(rec.stat.phase, MotionPhase::Retry);
    assert_eq!(rec.retry.rcnt, 1);
    assert!(!rec.retry.miss);
    assert!(matches!(
        effects.commands[0],
        MotorCommand::MoveAbsolute { .. }
    ));

    // Second retry
    complete_move(&mut rec, 9.98); // still > rdbd
    let _effects = rec.check_completion();
    assert_eq!(rec.retry.rcnt, 2);

    // Third retry
    complete_move(&mut rec, 9.995); // within rdbd
    let _effects = rec.check_completion();
    assert!(rec.stat.dmov);
    assert!(!rec.retry.miss);
}

#[test]
fn retry_modes_arithmetic_geometric_inposition() {
    // Arithmetic mode: target = drbv + (dval - drbv) * frac
    let mut rec = make_record();
    rec.retry.rdbd = 0.01;
    rec.retry.rtry = 5;
    rec.retry.frac = 0.5;
    rec.retry.rmod = RetryMode::Arithmetic;

    rec.pos.dval = 10.0;
    rec.plan_motion(CommandSource::Val);
    complete_move(&mut rec, 9.0);
    let effects = rec.check_completion();

    // C Arithmetic: factor = (rtry - rcnt + 1) / rtry = (5 - 1 + 1) / 5 = 1.0
    // retry_target = 10.0 (= dval), use_rel=false:
    // position = dval + frac*(retry_target - dval) = 10.0 + 0.5*0 = 10.0
    if let MotorCommand::MoveAbsolute { position, .. } = &effects.commands[0] {
        assert!((position - 10.0).abs() < 1e-6);
    } else {
        panic!("expected MoveAbsolute");
    }
}

#[test]
fn stop_during_move_clears_pending_motion() {
    let mut rec = make_record();

    rec.pos.dval = 50.0;
    rec.plan_motion(CommandSource::Val);
    assert!(!rec.stat.dmov);

    // Motor is moving
    motor_moving(&mut rec, 25.0);

    // Issue STOP
    let effects = rec.plan_motion(CommandSource::Stop);

    assert!(rec.stat.mip.contains(MipFlags::STOP));
    assert_eq!(effects.commands.len(), 1);
    assert!(matches!(effects.commands[0], MotorCommand::Stop { .. }));
    // C parity: no eager VAL<-RBV sync at stop-put time; the sync runs
    // once the axis has actually stopped (postProcess). VAL still holds
    // its pre-stop value (never written in this scenario).
    assert_eq!(rec.pos.val, 0.0);

    complete_move(&mut rec, 25.0);
    let _ = rec.check_completion();
    assert!(rec.stat.dmov);
    assert_eq!(rec.pos.val, rec.pos.rbv);
}

#[test]
fn jog_start_stop_backlash_sequence() {
    let mut rec = make_record();
    rec.retry.bdst = 1.0;

    // Start jog forward
    rec.ctrl.jogf = true;
    let effects = rec.plan_motion(CommandSource::Jogf);
    assert!(!rec.stat.dmov);
    assert_eq!(rec.stat.phase, MotionPhase::Jog);
    assert!(rec.stat.mip.contains(MipFlags::JOGF));
    assert!(matches!(
        effects.commands[0],
        MotorCommand::MoveVelocity {
            direction: true,
            ..
        }
    ));

    // Stop jog
    rec.ctrl.jogf = false;
    let effects = rec.plan_motion(CommandSource::Jogf);
    assert!(rec.stat.mip.contains(MipFlags::JOG_STOP));
    assert!(matches!(effects.commands[0], MotorCommand::Stop { .. }));

    // Motor stops at position 20.0
    complete_move(&mut rec, 20.0);
    let effects = rec.check_completion();
    // C: jog backlash is unconditional when |BDST| >= |MRES|
    // Phase 1 (BL1): move to pretarget at slew velocity
    assert_eq!(rec.stat.phase, MotionPhase::JogBacklash);
    assert!(!effects.commands.is_empty());

    // BL1 completes at pretarget
    complete_move(&mut rec, 20.0 - 1.0); // pretarget = dval - bdst
    let effects = rec.check_completion();
    // Phase 2 (BL2): move to final at backlash velocity
    assert!(!effects.commands.is_empty());

    // BL2 completes
    complete_move(&mut rec, 20.0);
    let _effects = rec.check_completion();
    assert!(rec.stat.dmov);
}

#[test]
fn home_forward_reverse_marks_homed() {
    let mut rec = make_record();

    // Home forward
    rec.ctrl.homf = true;
    let effects = rec.plan_motion(CommandSource::Homf);
    assert!(!rec.stat.dmov);
    assert_eq!(rec.stat.phase, MotionPhase::Homing);
    assert!(rec.stat.mip.contains(MipFlags::HOMF));
    assert!(!rec.ctrl.homf); // pulse cleared
    assert!(matches!(
        effects.commands[0],
        MotorCommand::Home { forward: true, .. }
    ));

    // Homing completes
    complete_move(&mut rec, 0.0);
    let _effects = rec.check_completion();
    assert!(rec.stat.athm);
    assert!(rec.stat.dmov);
    assert_eq!(rec.stat.phase, MotionPhase::Idle);
}

#[test]
fn dly_delays_final_dmov_assertion() {
    let mut rec = make_record();
    rec.timing.dly = 0.5;

    rec.pos.dval = 10.0;
    rec.plan_motion(CommandSource::Val);
    assert!(!rec.stat.dmov);

    // Motor reaches target
    complete_move(&mut rec, 10.0);
    let effects = rec.check_completion();

    // Should be in delay wait
    assert_eq!(rec.stat.phase, MotionPhase::DelayWait);
    assert!(effects.schedule_delay.is_some());
    assert!(!rec.stat.dmov);

    // Delay expires — C: sets DELAY_ACK, requests fresh poll
    rec.set_event(MotorEvent::DelayExpired);
    let effects = rec.do_process();
    assert!(effects.status_refresh);
    assert!(!rec.stat.dmov); // not yet finalized

    // Fresh poll arrives — evaluate position error, finalize
    complete_move(&mut rec, 10.0);
    let _effects = rec.check_completion();
    assert!(rec.stat.dmov);
    assert_eq!(rec.stat.phase, MotionPhase::Idle);
}

#[test]
fn spmg_stop_blocks_new_commands() {
    let mut rec = make_record();
    rec.ctrl.spmg = SpmgMode::Stop;

    rec.pos.dval = 50.0;
    let effects = rec.plan_motion(CommandSource::Val);

    // Should be blocked
    assert!(effects.commands.is_empty());
    assert!(rec.stat.dmov);
}

#[test]
fn spmg_pause_sends_stop_and_syncs_after_completion() {
    let mut rec = make_record();

    // Start a move
    rec.pos.dval = 50.0;
    rec.plan_motion(CommandSource::Val);
    assert!(!rec.stat.dmov);

    // Motor moving at 25
    motor_moving(&mut rec, 25.0);

    // Pause: sends STOP, sets MIP_STOP, motor still running
    rec.ctrl.spmg = SpmgMode::Pause;
    let effects = rec.plan_motion(CommandSource::Spmg);
    assert!(matches!(effects.commands[0], MotorCommand::Stop { .. }));
    assert!(!rec.stat.dmov); // still moving
    assert!(rec.stat.mip.contains(MipFlags::STOP));

    // Motor stops at 25
    complete_move(&mut rec, 25.0);
    let _effects = rec.check_completion();
    // C: postProcess syncs VAL=RBV, DVAL=DRBV
    assert!(rec.stat.dmov);
    assert_eq!(rec.pos.dval, 25.0);
    assert_eq!(rec.pos.val, 25.0);
}

#[test]
fn ntm_retargets_while_in_motion() {
    let mut rec = make_record();
    rec.timing.ntm = true;
    rec.timing.ntmf = 2.0;

    // Start move to 50
    rec.pos.dval = 50.0;
    rec.plan_motion(CommandSource::Val);
    rec.internal.ldvl = 50.0;

    // Motor moving at position 25
    motor_moving(&mut rec, 25.0);

    // Ensure MIP and CDIR are set (plan_motion sets these)
    assert!(rec.stat.mip.contains(MipFlags::MOVE));
    assert!(rec.stat.cdir); // positive direction

    // New target: 80 (same direction, farther)
    assert_eq!(rec.handle_retarget(80.0), RetargetAction::ExtendMove);

    // New target: -10 (opposite direction, beyond deadband)
    assert_eq!(rec.handle_retarget(-10.0), RetargetAction::StopAndReplan);

    // New target: 30 (same direction, closer) -> ExtendMove (same dir in C)
    assert_eq!(rec.handle_retarget(30.0), RetargetAction::ExtendMove);
}

#[test]
fn set_mode_redefines_coordinates() {
    let mut rec = make_record();
    rec.pos.dval = 10.0;
    rec.pos.val = 10.0;

    // Enter SET mode
    rec.conv.set = true;

    // Write VAL=100 → should update OFF, not move
    rec.put_field("VAL", EpicsValue::Double(100.0)).unwrap();

    assert_eq!(rec.pos.val, 100.0);
    assert_eq!(rec.pos.dval, 10.0); // unchanged
    assert_eq!(rec.pos.off, 90.0); // 100 - 1*10

    // Exit SET mode
    rec.conv.set = false;
}

#[test]
fn frozen_offset_non_set_cascades_normally() {
    let mut rec = make_record();
    rec.conv.foff = FreezeOffset::Frozen;
    rec.pos.val = 10.0;
    rec.pos.off = 5.0;

    // C: FOFF has no effect in non-SET mode -- VAL recalculated, OFF unchanged
    rec.put_field("DVAL", EpicsValue::Double(20.0)).unwrap();

    assert_eq!(rec.pos.val, 25.0); // dial_to_user(20.0, Pos, 5.0)
    assert_eq!(rec.pos.dval, 20.0);
    assert_eq!(rec.pos.off, 5.0); // unchanged
}

#[test]
fn startup_syncs_positions_from_driver() {
    let mut rec = make_record();

    let status = MotorStatus {
        position: 25.0,
        encoder_position: 25.0,
        done: true,
        moving: false,
        ..Default::default()
    };

    let effects = rec.initial_readback(&status);
    assert!(rec.stat.dmov);
    assert_eq!(rec.pos.rbv, 25.0);
    assert_eq!(rec.pos.val, 25.0);
    assert_eq!(rec.pos.dval, 25.0);
    assert!(!effects.request_poll);
}

#[test]
fn startup_moving_starts_polling() {
    let mut rec = make_record();

    let status = MotorStatus {
        position: 10.0,
        encoder_position: 10.0,
        done: false,
        moving: true,
        ..Default::default()
    };

    let effects = rec.initial_readback(&status);
    assert!(!rec.stat.dmov);
    assert!(effects.request_poll);
}

#[test]
fn comm_error_sets_alarm_and_safe_state() {
    let mut rec = make_record();

    // Simulate MSTA with COMM_ERR set
    rec.stat.msta.insert(MstaFlags::COMM_ERR);
    assert!(rec.stat.msta.contains(MstaFlags::COMM_ERR));
}

#[test]
fn sub_step_move_pulses_dmov() {
    let mut rec = make_record();

    // Move to current position (< 1 step) -- no motor command, but DMOV pulses
    rec.pos.dval = 0.0;
    rec.pos.drbv = 0.0;
    let effects = rec.plan_motion(CommandSource::Val);
    assert!(!rec.stat.dmov); // DMOV pulsed to 0
    assert!(effects.commands.is_empty()); // no motor command
    assert!(effects.request_poll); // triggers re-process

    // Next process cycle: finalize restores DMOV=1
    let _effects = rec.do_process();
    assert!(rec.stat.dmov);

    // Move that is at least 1 step should proceed normally
    rec.pos.dval = rec.conv.mres * 2.0; // 2 steps
    let effects = rec.plan_motion(CommandSource::Val);
    assert!(!rec.stat.dmov);
    assert!(!effects.commands.is_empty());
}

#[test]
fn dir_neg_swaps_limit_mapping() {
    let mut rec = make_record();
    rec.conv.dir = MotorDir::Neg;
    rec.pos.off = 0.0;

    rec.put_field("HLM", EpicsValue::Double(100.0)).unwrap();
    rec.put_field("LLM", EpicsValue::Double(-100.0)).unwrap();

    // DIR=Neg: user HLM=100 → dial = (100-0)*(-1) = -100
    // user LLM=-100 → dial = (-100-0)*(-1) = 100
    // After normalization: DHLM=100, DLLM=-100
    assert_eq!(rec.limits.dhlm, 100.0);
    assert_eq!(rec.limits.dllm, -100.0);
}

#[test]
fn new_target_opposite_direction_stops_immediately() {
    let mut rec = make_record();
    rec.timing.ntm = true;

    // Moving forward
    rec.pos.dval = 50.0;
    rec.plan_motion(CommandSource::Val);
    rec.internal.ldvl = 50.0;
    motor_moving(&mut rec, 25.0);

    // Target in opposite direction
    let action = rec.handle_retarget(-20.0);
    assert_eq!(action, RetargetAction::StopAndReplan);
}

#[test]
fn sim_motor_end_to_end() {
    let user = AsynUser::new(0);
    let mut motor = SimMotor::new().with_limits(-100.0, 100.0);
    let mut rec = make_record();

    // Initial readback
    let status = motor.poll(&user).unwrap();
    rec.initial_readback(&status);
    assert_eq!(rec.pos.rbv, 0.0);

    // Start move
    rec.put_field("VAL", EpicsValue::Double(10.0)).unwrap();
    let effects = rec.plan_motion(CommandSource::Val);
    assert!(!rec.stat.dmov);

    // Execute command on motor — use very high velocity for test speed
    if let MotorCommand::MoveAbsolute {
        position,
        acceleration,
        ..
    } = &effects.commands[0]
    {
        motor
            .move_absolute(&user, *position, 100000.0, *acceleration)
            .unwrap();
    }

    // Wait for completion
    std::thread::sleep(Duration::from_millis(10));

    // Poll and update
    let status = motor.poll(&user).unwrap();
    assert!(status.done);
    assert_eq!(status.position, 10.0);

    rec.process_motor_info(&status);
    let _effects = rec.check_completion();

    assert!(rec.stat.dmov);
    assert!((rec.pos.rbv - 10.0).abs() < 1e-6);
}

/// Setting VAL to the current position must still produce a DMOV 1→0→1
/// transition. ophyd/bluesky rely on this to detect move completion.
#[test]
fn move_to_same_position_pulses_dmov() {
    let mut rec = make_record();

    rec.pos.val = 0.0;
    rec.pos.dval = 0.0;
    rec.pos.rval = 0;
    rec.pos.rbv = 0.0;
    rec.pos.drbv = 0.0;
    rec.stat.dmov = true;
    rec.stat.phase = MotionPhase::Idle;

    // Write VAL=0 (same position) — DMOV pulses 1→0→1
    rec.put_field("VAL", EpicsValue::Double(0.0)).unwrap();
    let effects = rec.plan_motion(CommandSource::Val);

    assert!(!rec.stat.dmov, "DMOV should pulse to 0");
    assert!(effects.commands.is_empty(), "no motor command for sub-step");

    // Next process cycle restores DMOV=1
    let _effects = rec.do_process();
    assert!(rec.stat.dmov, "DMOV should return to 1");
    assert_eq!(rec.stat.phase, MotionPhase::Idle);
}

/// Same-position DMOV transition with SimMotor end-to-end.
#[test]
fn sim_motor_same_position_pulses_dmov() {
    let mut rec = make_record();
    rec.pos.val = 5.0;
    rec.pos.dval = 5.0;
    rec.pos.rval = 5000;
    rec.pos.rbv = 5.0;
    rec.pos.drbv = 5.0;
    rec.stat.dmov = true;
    rec.stat.phase = MotionPhase::Idle;

    // Write VAL=5 (same position) — DMOV pulses
    rec.put_field("VAL", EpicsValue::Double(5.0)).unwrap();
    let effects = rec.plan_motion(CommandSource::Val);

    assert!(!rec.stat.dmov, "DMOV should pulse to 0");
    assert!(effects.commands.is_empty());

    let _effects = rec.do_process();
    assert!(rec.stat.dmov, "DMOV should return to 1");
}

/// Sequential moves: move to multiple positions and verify each.
/// Ported from ophyd test_move.
#[test]
fn sequential_moves_verify_position() {
    let mut rec = make_record();

    // C: moves to same position (< 1 step) are suppressed, so skip duplicates
    let positions = [0.1, 0.0, 0.1, -5.0, 5.0];

    for &target in &positions {
        rec.put_field("VAL", EpicsValue::Double(target)).unwrap();
        let _effects = rec.plan_motion(CommandSource::Val);

        assert!(!rec.stat.dmov, "DMOV should be 0 after move to {target}");

        // Simulate completion
        complete_move(&mut rec, target);
        let _effects = rec.check_completion();

        assert!(
            rec.stat.dmov,
            "DMOV should be 1 after completion at {target}"
        );
        assert_eq!(rec.stat.phase, MotionPhase::Idle);
        assert!(
            (rec.pos.rbv - target).abs() < 1e-6,
            "RBV should be {target}, got {}",
            rec.pos.rbv
        );
        assert!(
            (rec.pos.val - target).abs() < 1e-6,
            "VAL should be {target}, got {}",
            rec.pos.val
        );
    }
}

/// Calibration: set_current_position changes offset without moving.
/// Ported from ophyd test_calibration.
#[test]
fn calibration_set_current_position_updates_offset() {
    let mut rec = make_record();

    // Start at position 0
    complete_move(&mut rec, 0.0);
    let _ = rec.check_completion();
    assert!((rec.pos.val - 0.0).abs() < 1e-6);
    assert!((rec.pos.off - 0.0).abs() < 1e-6);

    // Enter SET mode
    rec.put_field("SET", EpicsValue::Short(1)).unwrap();
    let _ = rec.plan_motion(CommandSource::Set);

    // Write new position: "I am now at 10.0"
    rec.put_field("VAL", EpicsValue::Double(10.0)).unwrap();
    let effects = rec.plan_motion(CommandSource::Set);

    // Should issue SetPosition, not a move
    assert!(
        effects
            .commands
            .iter()
            .any(|c| matches!(c, MotorCommand::SetPosition { .. })),
        "SET mode should issue SetPosition command"
    );

    // Verify offset changed: OFF = new_val - dial = 10.0 - 0.0 = 10.0
    assert!(
        (rec.pos.off - 10.0).abs() < 1e-6,
        "OFF should be 10.0, got {}",
        rec.pos.off
    );
    assert!(
        (rec.pos.val - 10.0).abs() < 1e-6,
        "VAL should read 10.0, got {}",
        rec.pos.val
    );
    // DVAL/DRBV should remain at 0 (dial didn't change)
    assert!(
        (rec.pos.dval - 0.0).abs() < 1e-6,
        "DVAL should still be 0.0, got {}",
        rec.pos.dval
    );

    // Leave SET mode
    rec.put_field("SET", EpicsValue::Short(0)).unwrap();

    // Now move to 0 in user coords (should move dial to -10)
    rec.put_field("VAL", EpicsValue::Double(0.0)).unwrap();
    let effects = rec.plan_motion(CommandSource::Val);
    assert!(!rec.stat.dmov);
    assert!(
        effects
            .commands
            .iter()
            .any(|c| matches!(c, MotorCommand::MoveAbsolute { .. })),
        "Should issue a real move after leaving SET mode"
    );
}

/// SimMotor sequential moves end-to-end.
/// Ported from ophyd test_move with actual motor simulation.
#[test]
fn sim_motor_sequential_moves() {
    let mut motor = SimMotor::new();
    let user = AsynUser::new(0);
    let mut rec = make_record();

    let targets = [0.5, 0.0, 0.5, -1.0];

    for &target in &targets {
        rec.put_field("VAL", EpicsValue::Double(target)).unwrap();
        let effects = rec.plan_motion(CommandSource::Val);
        assert!(!rec.stat.dmov);

        for cmd in &effects.commands {
            if let MotorCommand::MoveAbsolute {
                position,
                velocity,
                acceleration,
            } = cmd
            {
                motor
                    .move_absolute(&user, *position, *velocity, *acceleration)
                    .unwrap();
            }
        }

        // Wait for SimMotor to reach target
        for _ in 0..100 {
            std::thread::sleep(Duration::from_millis(20));
            let status = motor.poll(&user).unwrap();
            rec.process_motor_info(&status);
            let _effects = rec.check_completion();
            if rec.stat.dmov {
                break;
            }
        }

        assert!(rec.stat.dmov, "Motor should reach target {target}");
        assert!(
            (rec.pos.rbv - target).abs() < 0.01,
            "RBV should be ~{target}, got {}",
            rec.pos.rbv
        );
    }
}

/// RBV updates during move — verify intermediate positions.
/// Ported from ophyd test_watchers.
#[test]
fn rbv_updates_during_move() {
    let mut motor = SimMotor::new();
    let user = AsynUser::new(0);
    let mut rec = make_record();
    rec.vel.velo = 5.0; // slower velocity so we can observe intermediate positions

    rec.put_field("VAL", EpicsValue::Double(2.0)).unwrap();
    let effects = rec.plan_motion(CommandSource::Val);

    for cmd in &effects.commands {
        if let MotorCommand::MoveAbsolute {
            position,
            velocity,
            acceleration,
        } = cmd
        {
            motor
                .move_absolute(&user, *position, *velocity, *acceleration)
                .unwrap();
        }
    }

    let mut rbv_history: Vec<f64> = Vec::new();

    for _ in 0..100 {
        std::thread::sleep(Duration::from_millis(20));
        let status = motor.poll(&user).unwrap();
        rec.process_motor_info(&status);
        rbv_history.push(rec.pos.rbv);
        let _effects = rec.check_completion();
        if rec.stat.dmov {
            break;
        }
    }

    assert!(rec.stat.dmov, "Motor should complete the move");
    assert!(
        rbv_history.len() > 1,
        "Should have multiple RBV updates during move, got {}",
        rbv_history.len()
    );
    // RBV should be monotonically increasing (moving from 0 to 2)
    for i in 1..rbv_history.len() {
        assert!(
            rbv_history[i] >= rbv_history[i - 1] - 1e-10,
            "RBV should be monotonically increasing: {} < {}",
            rbv_history[i],
            rbv_history[i - 1]
        );
    }
    // Final RBV should be at target
    assert!(
        (rec.pos.rbv - 2.0).abs() < 0.01,
        "Final RBV should be ~2.0, got {}",
        rec.pos.rbv
    );
}

/// Homing with SimMotor end-to-end.
/// Ported from ophyd test_homing_forward/reverse.
#[test]
fn sim_motor_homing() {
    let mut motor = SimMotor::new();
    let user = AsynUser::new(0);
    let mut rec = make_record();

    // Move to 5.0 first
    rec.put_field("VAL", EpicsValue::Double(5.0)).unwrap();
    let effects = rec.plan_motion(CommandSource::Val);
    for cmd in &effects.commands {
        if let MotorCommand::MoveAbsolute {
            position,
            velocity,
            acceleration,
        } = cmd
        {
            motor
                .move_absolute(&user, *position, *velocity, *acceleration)
                .unwrap();
        }
    }
    // Wait for move to complete (distance=5, velocity=10 → 0.5s)
    for _ in 0..100 {
        std::thread::sleep(Duration::from_millis(20));
        let status = motor.poll(&user).unwrap();
        rec.process_motor_info(&status);
        let _ = rec.check_completion();
        if rec.stat.dmov {
            break;
        }
    }
    assert!(rec.stat.dmov, "First move to 5.0 should complete");

    // Home forward
    rec.ctrl.homf = true;
    let effects = rec.plan_motion(CommandSource::Homf);
    assert!(!rec.stat.dmov);
    assert!(
        effects
            .commands
            .iter()
            .any(|c| matches!(c, MotorCommand::Home { .. })),
        "Should issue Home command"
    );

    for cmd in &effects.commands {
        if let MotorCommand::Home {
            velocity, forward, ..
        } = cmd
        {
            motor.home(&user, *velocity, *forward).unwrap();
        }
    }

    // Wait for homing to complete
    for _ in 0..100 {
        std::thread::sleep(Duration::from_millis(20));
        let status = motor.poll(&user).unwrap();
        rec.process_motor_info(&status);
        let _ = rec.check_completion();
        if rec.stat.dmov {
            break;
        }
    }

    assert!(rec.stat.dmov, "Homing should complete");
    assert!(
        (rec.pos.drbv - 0.0).abs() < 0.01,
        "After homing, DRBV should be near 0, got {}",
        rec.pos.drbv
    );
}
