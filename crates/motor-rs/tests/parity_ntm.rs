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
fn val_during_motion_same_direction_accepted() {
    let mut rec = make_record();

    // Start move to 50
    rec.put_field("VAL", EpicsValue::Double(50.0)).unwrap();
    rec.plan_motion(CommandSource::Val);
    rec.internal.ldvl = 50.0;
    assert!(!rec.stat.dmov);
    assert_eq!(rec.stat.phase, MotionPhase::MainMove);

    // Motor moving at 25
    motor_moving(&mut rec, 25.0);

    // New target: 80 (same direction, farther) → ExtendMove.
    // Must re-emit a move command so the driver actually retargets.
    // Without this, the driver keeps the old target and motor stops
    // at the original target (RTRY=0 won't save us).
    rec.put_field("VAL", EpicsValue::Double(80.0)).unwrap();
    let effects = rec.plan_motion(CommandSource::Val);

    // A new MoveAbsolute (or MoveRelative with equivalent target)
    // must be emitted to the driver at the new target.
    assert_eq!(effects.commands.len(), 1, "expected one move command");
    match &effects.commands[0] {
        MotorCommand::MoveAbsolute { position, .. } => {
            assert!(
                (*position - 80.0).abs() < 1e-6,
                "abs target should be 80, got {position}"
            );
        }
        MotorCommand::MoveRelative { distance, .. } => {
            // Relative move: current drbv is 25, so distance should be 55.
            assert!(
                (*distance - 55.0).abs() < 1e-6,
                "rel distance should be 55, got {distance}"
            );
        }
        other => panic!("expected Move command, got {other:?}"),
    }
    // C parity: ldvl is updated to the newly dispatched target
    // (motorRecord.cc:2469 load_pos on re-emitted move). This keeps
    // is_preferred_direction comparing against the most recent target
    // across successive in-flight retargets.
    assert_eq!(rec.internal.ldvl, 80.0);
    // Motion continues: DMOV stays 0 — C's only FLNK gate
    // (motorRecord.cc:1509-1510 fires recGblFwdLink iff dmov != 0).
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

/// BUG 3 regression: a VAL write during motion with NTM=No must still
/// re-issue a move. C `do_work` (motorRecord.cc:2241) re-runs the move
/// block on every `dval != ldvl || !dmov` — completely independent of
/// NTM. NTM (motorRecord.cc:1327-1331) gates only the opposite-direction
/// `STOP_AXIS` in the `process()` `movn` block.
///
/// NTM has no `initial()` in `motorRecord.dbd`, so it defaults to No —
/// meaning a buggy `if !ntm { Ignore }` gate would silently discard
/// EVERY mid-motion VAL write on a default-configured record.
#[test]
fn ntm_false_same_direction_retarget_still_reissues_move() {
    let mut rec = make_record();
    rec.timing.ntm = false;

    rec.put_field("VAL", EpicsValue::Double(50.0)).unwrap();
    rec.plan_motion(CommandSource::Val);
    rec.internal.ldvl = 50.0;

    motor_moving(&mut rec, 25.0);

    // New target while NTM=No, same direction (80 > 25, moving +).
    // C do_work re-issues the move regardless of NTM.
    rec.put_field("VAL", EpicsValue::Double(80.0)).unwrap();
    let effects = rec.plan_motion(CommandSource::Val);

    assert_eq!(
        effects.commands.len(),
        1,
        "NTM=No write during motion must still re-issue a move"
    );
    let new_target = match &effects.commands[0] {
        MotorCommand::MoveAbsolute { position, .. } => *position,
        MotorCommand::MoveRelative { distance, .. } => 25.0 + distance,
        other => panic!("expected Move command, got {other:?}"),
    };
    assert!(
        (new_target - 80.0).abs() < 1e-6,
        "retarget should reach 80, got {new_target}"
    );
    assert!(!rec.stat.dmov);
}

/// BUG 3 regression: with NTM=No, an opposite-direction VAL write during
/// motion must re-issue a move directly (no stop-and-replan). C do_work
/// re-issues; the opposite-direction `STOP_AXIS` path in the `movn` block
/// is gated on `ntm == menuYesNoYES` and therefore does NOT fire here.
#[test]
fn ntm_false_opposite_direction_retarget_reissues_without_stop() {
    let mut rec = make_record();
    rec.timing.ntm = false;

    rec.put_field("VAL", EpicsValue::Double(50.0)).unwrap();
    rec.plan_motion(CommandSource::Val);
    rec.internal.ldvl = 50.0;

    motor_moving(&mut rec, 25.0);
    // Moving toward +50 from 25 → commanded direction is positive.
    rec.stat.cdir = true;

    // Opposite-direction target (-20) while NTM=No. C does NOT stop
    // first (the STOP_AXIS path requires NTM=Yes); do_work re-issues
    // a move straight to the new target.
    rec.put_field("VAL", EpicsValue::Double(-20.0)).unwrap();
    let effects = rec.plan_motion(CommandSource::Val);

    assert_eq!(effects.commands.len(), 1, "expected one command");
    assert!(
        !matches!(effects.commands[0], MotorCommand::Stop { .. }),
        "NTM=No must NOT stop-and-replan; it re-issues a move directly"
    );
    assert!(
        !rec.stat.mip.contains(MipFlags::STOP),
        "MIP_STOP must not be set for an NTM=No retarget"
    );
    let new_target = match &effects.commands[0] {
        MotorCommand::MoveAbsolute { position, .. } => *position,
        MotorCommand::MoveRelative { distance, .. } => 25.0 + distance,
        other => panic!("expected Move command, got {other:?}"),
    };
    assert!(
        (new_target - (-20.0)).abs() < 1e-6,
        "retarget should reach -20, got {new_target}"
    );
}

#[test]
fn retarget_during_backlash_cancels_pending() {
    let mut rec = make_record();
    rec.retry.bdst = 1.0;

    // Start move with backlash (moving negative with positive BDST)
    rec.put_field("VAL", EpicsValue::Double(-10.0)).unwrap();
    rec.plan_motion(CommandSource::Val);
    rec.internal.ldvl = -10.0;
    assert!(rec.internal.backlash_pending);
    assert_eq!(rec.stat.phase, MotionPhase::MainMove);

    motor_moving(&mut rec, -5.0);

    // Explicitly set CDIR to match the negative move direction
    // (CDIR may be overwritten by process_motor_info's TDIR update)
    rec.stat.cdir = false; // moving negative

    // Retarget to opposite direction (20.0 vs original -10.0)
    rec.put_field("VAL", EpicsValue::Double(20.0)).unwrap();
    let effects = rec.plan_motion(CommandSource::Val);

    // Opposite direction retarget: stop-and-replan
    assert!(rec.stat.mip.contains(MipFlags::STOP));
    assert!(!rec.internal.backlash_pending);
    assert!(matches!(effects.commands[0], MotorCommand::Stop { .. }));
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
    assert_eq!(rec.stat.phase, MotionPhase::MainMove);
    assert!(
        rec.stat.mip.contains(MipFlags::RETRY),
        "retry marked in MIP"
    );
    assert_eq!(rec.retry.rcnt, 1);

    // Motor retrying
    motor_moving(&mut rec, 9.7);

    // Retarget to opposite direction during retry (requires stop-and-replan)
    rec.put_field("VAL", EpicsValue::Double(-20.0)).unwrap();
    let effects = rec.plan_motion(CommandSource::Val);

    // RCNT should be reset, stop issued
    assert_eq!(rec.retry.rcnt, 0);
    assert!(matches!(effects.commands[0], MotorCommand::Stop { .. }));
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
fn spmg_pause_preserves_target_and_resumes_on_go() {
    let mut rec = make_record();

    // Start move
    rec.put_field("VAL", EpicsValue::Double(50.0)).unwrap();
    rec.plan_motion(CommandSource::Val);
    motor_moving(&mut rec, 25.0);

    // Pause: sends STOP, sets MIP_STOP, but motor is still moving
    rec.ctrl.spmg = SpmgMode::Pause;
    let effects = rec.plan_motion(CommandSource::Spmg);
    assert!(!rec.stat.dmov); // motor still moving
    assert!(rec.stat.mip.contains(MipFlags::STOP));
    assert!(matches!(effects.commands[0], MotorCommand::Stop { .. }));

    // Motor stops at 25. C: Pause never set pp, so postProcess is skipped
    // (1383-1402) — the target survives in VAL/DVAL — and maybeRetry
    // (1077-1082) arms MIP_RETRY with DMOV left FALSE (1356 UNMARKed).
    complete_move(&mut rec, 25.0);
    let effects = rec.check_completion();
    assert!(effects.commands.is_empty());
    assert!(!rec.stat.dmov); // paused move never reports done
    assert_eq!(rec.stat.mip, MipFlags::RETRY);
    assert_eq!(rec.pos.dval, 50.0); // target preserved, not synced
    assert_eq!(rec.pos.val, 50.0);
    assert_eq!(rec.retry.rcnt, 1); // C: each paused stop counts a retry

    // Go: the move-block pass (2241) re-fires on !dmov toward the
    // preserved DVAL, keeping the retry count (C 2351-2356). C's Go arm
    // (1912-1926) leaves mip == MIP_RETRY untouched, so the dispatch
    // (2461 mip |= MIP_MOVE) resumes as a marked retry.
    rec.ctrl.spmg = SpmgMode::Go;
    let effects = rec.plan_motion(CommandSource::Spmg);
    assert_eq!(effects.commands.len(), 1);
    assert!(matches!(
        effects.commands[0],
        MotorCommand::MoveAbsolute { position, .. } if (position - 50.0).abs() < 1e-9
    ));
    assert_eq!(rec.stat.mip, MipFlags::MOVE | MipFlags::RETRY);
    assert_eq!(rec.retry.rcnt, 1);

    // The resumed move completes at the target.
    complete_move(&mut rec, 50.0);
    rec.check_completion();
    assert!(rec.stat.dmov);
    assert_eq!(rec.pos.dval, 50.0);
}

#[test]
fn same_direction_retarget_reaches_new_target_without_retry() {
    // Regression: caput 25 then (mid-motion) caput 45 on same direction must
    // cause the motor to actually reach 45, not stop at 25. Previously the
    // ExtendMove branch updated DVAL silently and the driver kept going to
    // 25, so with RTRY=0 (no retry to save us) the motor stopped at 25.
    let mut rec = make_record();
    rec.retry.rtry = 0; // disable retry to prove we don't depend on it

    // Move from 0 to 25
    rec.put_field("VAL", EpicsValue::Double(25.0)).unwrap();
    let effects = rec.plan_motion(CommandSource::Val);
    assert_eq!(effects.commands.len(), 1);
    rec.internal.ldvl = 25.0;

    // Motor moving at 15 (mid-flight)
    motor_moving(&mut rec, 15.0);
    assert!(!rec.stat.dmov);

    // User retargets to 45 (same direction, further)
    rec.put_field("VAL", EpicsValue::Double(45.0)).unwrap();
    let effects = rec.plan_motion(CommandSource::Val);

    // Must issue a new move command to the new target.
    assert_eq!(
        effects.commands.len(),
        1,
        "retarget must emit a new move command"
    );
    let new_target = match &effects.commands[0] {
        MotorCommand::MoveAbsolute { position, .. } => *position,
        MotorCommand::MoveRelative { distance, .. } => 15.0 + distance,
        other => panic!("expected Move command, got {other:?}"),
    };
    assert!(
        (new_target - 45.0).abs() < 1e-6,
        "new target should be 45, got {new_target}"
    );

    // Simulate driver accepting the retarget and reaching 45.
    complete_move(&mut rec, 45.0);
    let _effects = rec.check_completion();

    assert!(rec.stat.dmov, "motor should be done after reaching 45");
    assert!(
        (rec.pos.drbv - 45.0).abs() < 1e-6,
        "DRBV should be 45, got {}",
        rec.pos.drbv
    );
}

#[test]
fn same_direction_retarget_safety_net_replans_if_driver_ignores_inflight() {
    // Safety net: even if the driver silently ignores the in-flight retarget
    // and stops at the OLD target, completion-time verification must replan
    // to the new target (without depending on RTRY/RDBD).
    let mut rec = make_record();
    rec.retry.rtry = 0; // no retries available
    rec.retry.rdbd = 0.0; // retry gating disabled

    // Start move to 25
    rec.put_field("VAL", EpicsValue::Double(25.0)).unwrap();
    rec.plan_motion(CommandSource::Val);
    rec.internal.ldvl = 25.0;

    motor_moving(&mut rec, 15.0);

    // Mid-motion retarget to 45 (same direction) → ExtendMove arms safety net
    rec.put_field("VAL", EpicsValue::Double(45.0)).unwrap();
    let effects = rec.plan_motion(CommandSource::Val);
    assert_eq!(effects.commands.len(), 1, "ExtendMove must emit move");
    assert!(
        rec.internal.verify_retarget_on_completion,
        "verify flag must be armed"
    );

    // Simulate driver IGNORING the in-flight retarget and stopping at 25.
    complete_move(&mut rec, 25.0);
    let effects = rec.check_completion();

    // Safety net must have replanned to 45.
    assert!(
        !rec.internal.verify_retarget_on_completion,
        "verify flag must be cleared after check"
    );
    assert!(
        !rec.stat.dmov,
        "motor must NOT be marked done — replan issued"
    );
    assert_eq!(
        effects.commands.len(),
        1,
        "safety-net replan must emit a move command"
    );
    let new_target = match &effects.commands[0] {
        MotorCommand::MoveAbsolute { position, .. } => *position,
        MotorCommand::MoveRelative { distance, .. } => 25.0 + distance,
        other => panic!("expected Move command, got {other:?}"),
    };
    assert!(
        (new_target - 45.0).abs() < 1e-6,
        "safety-net target should be 45, got {new_target}"
    );

    // Driver reaches 45 on second attempt.
    complete_move(&mut rec, 45.0);
    let _effects = rec.check_completion();
    assert!(rec.stat.dmov);
    assert!((rec.pos.drbv - 45.0).abs() < 1e-6);
}

#[test]
fn dispatch_posts_rdif_without_a_poll() {
    // C move-block entry (motorRecord.cc:2248-2255): DIFF and RDIF are
    // recomputed from the requested target before dispatch — a VAL
    // write posts them immediately, not on the next poll.
    let mut rec = make_record();
    rec.pos.drbv = 0.0;

    rec.put_field("VAL", EpicsValue::Double(10.0)).unwrap();
    rec.plan_motion(CommandSource::Val);

    assert!((rec.pos.diff - 10.0).abs() < 1e-9);
    assert_eq!(rec.pos.rdif, 10_000, "RDIF = NINT(diff/mres) at dispatch");
}

#[test]
fn cdir_reads_forward_for_sub_half_step_negative_error() {
    // C 2526: cdir = (rdif < 0) ? 0 : 1 with rdif = NINT(diff/mres) —
    // the INTEGER raw error. A dispatch whose error is negative but
    // under half a step (rdif rounds to 0) reads FORWARD in C; deriving
    // CDIR from the f64 sign of diff read it backward.
    let mut rec = make_record();
    rec.pos.drbv = -0.0001;

    // npos = NINT(-0.5) = -1, rpos = NINT(-0.1) = 0: one step, so the
    // move dispatches; rdif = NINT(-0.4) = 0.
    rec.put_field("VAL", EpicsValue::Double(-0.0005)).unwrap();
    let effects = rec.plan_motion(CommandSource::Val);
    assert!(
        effects
            .commands
            .iter()
            .any(|c| matches!(c, MotorCommand::MoveAbsolute { .. })),
        "setup: the sub-half-step move must dispatch, got {:?}",
        effects.commands
    );

    assert_eq!(rec.pos.rdif, 0);
    assert!(rec.stat.cdir, "rdif 0 reads forward (C 2526)");
}

#[test]
fn ntm_direction_sign_is_raw_frame_under_negative_mres() {
    // C 1303: sign_rdif = (rdif < 0) ? 0 : 1 with rdif = NINT(diff/mres)
    // — RAW frame, like CDIR. Under MRES < 0 a dial-NEGATIVE error is
    // raw-POSITIVE: comparing the dial sign against the raw CDIR would
    // misread a same-direction retarget as a reversal (and vice versa).
    let mut rec = make_record();
    rec.conv.mres = -0.001;

    // Move down from 0 to -50: raw error = -50/-0.001 > 0, so the
    // dispatch reads CDIR = forward (raw frame).
    rec.put_field("VAL", EpicsValue::Double(-50.0)).unwrap();
    rec.plan_motion(CommandSource::Val);
    assert!(rec.stat.cdir, "raw-frame CDIR forward under MRES < 0");

    motor_moving(&mut rec, -25.0);

    // Same dial direction (further down): raw sign still positive —
    // NOT a reversal. The dial-sign comparison would have stopped here.
    rec.put_field("VAL", EpicsValue::Double(-60.0)).unwrap();
    let effects = rec.plan_motion(CommandSource::Val);
    assert!(
        !effects
            .commands
            .iter()
            .any(|c| matches!(c, MotorCommand::Stop { .. })),
        "same-direction retarget must not NTM-stop, got {:?}",
        effects.commands
    );
    assert!(!rec.stat.mip.contains(MipFlags::STOP));

    // True reversal (back up to +40): raw sign flips — NTM stops.
    motor_moving(&mut rec, -30.0);
    rec.put_field("VAL", EpicsValue::Double(40.0)).unwrap();
    let effects = rec.plan_motion(CommandSource::Val);
    assert!(
        effects
            .commands
            .iter()
            .any(|c| matches!(c, MotorCommand::Stop { .. })),
        "opposite-direction retarget NTM-stops, got {:?}",
        effects.commands
    );
}

#[test]
fn poll_time_wrong_direction_stops_then_retries_target() {
    // C movn block (motorRecord.cc:1327-1342): a poll showing the axis
    // beyond the target — raw error sign opposite CDIR, |diff| past
    // ntmf*(|bdst|+rdbd) — stops it with pp = FALSE, so the target
    // survives; the stop completion's maybeRetry then re-dispatches in
    // the same pass (SPMG is Go, so do_work reaches the 2455 gate).
    let mut rec = make_record();
    rec.retry.rdbd = 0.1;
    rec.retry.rtry = 5;

    rec.put_field("VAL", EpicsValue::Double(50.0)).unwrap();
    rec.plan_motion(CommandSource::Val);
    assert!(rec.stat.cdir);

    // Poll: still moving, but the readback has run past the target.
    motor_moving(&mut rec, 55.0);
    let effects = rec.check_completion();
    assert!(
        effects
            .commands
            .iter()
            .any(|c| matches!(c, MotorCommand::Stop { .. })),
        "wrong-direction poll stops the axis, got {:?}",
        effects.commands
    );
    assert!(rec.stat.mip.contains(MipFlags::STOP));

    // Stop completes at 55: no postProcess sync (pp = FALSE) — the
    // target is intact and the retry re-dispatches toward it.
    complete_move(&mut rec, 55.0);
    let effects = rec.check_completion();
    assert_eq!(rec.pos.dval, 50.0, "target survives the NTM stop");
    assert!(rec.stat.mip.contains(MipFlags::RETRY));
    assert_eq!(rec.stat.phase, MotionPhase::MainMove);
    let MotorCommand::MoveAbsolute { position, .. } = &effects.commands[0] else {
        panic!("expected re-dispatch, got {:?}", effects.commands);
    };
    assert!((*position - 50.0).abs() < 1e-9, "got {position}");
}
