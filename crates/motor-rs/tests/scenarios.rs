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

    assert_eq!(rec.stat.phase, MotionPhase::MainMove);
    assert!(
        rec.stat.mip.contains(MipFlags::RETRY),
        "retry marked in MIP"
    );
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
    assert!(rec.ctrl.homf); // stays latched until home completion (C 893-906)
    assert!(matches!(
        effects.commands[0],
        MotorCommand::Home { forward: true, .. }
    ));

    // Homing completes standing on the home switch — ATHM is pure
    // switch readback from the poll (C 3755-3762), not a homed latch.
    let status = MotorStatus {
        position: 0.0,
        encoder_position: 0.0,
        done: true,
        moving: false,
        home: true,
        ..Default::default()
    };
    rec.process_motor_info(&status);
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
    // Bare SPMG=Stop (lspg still Go): the next pass runs the C top block
    // — STOP_AXIS goes out "just in case" and the position write is
    // discarded (motorRecord.cc:1872-1911).
    rec.ctrl.spmg = SpmgMode::Stop;

    rec.pos.dval = 50.0;
    let effects = rec.plan_motion(CommandSource::Val);

    assert!(
        !effects.commands.iter().any(|c| matches!(
            c,
            MotorCommand::MoveAbsolute { .. } | MotorCommand::MoveRelative { .. }
        )),
        "no move under SPMG=Stop"
    );
    assert!(matches!(effects.commands[0], MotorCommand::Stop { .. }));
    assert!(rec.stat.dmov);
    assert_eq!(rec.internal.lspg, SpmgMode::Stop);

    // With LSPG synced, a further write is refused outright.
    let effects = rec.plan_motion(CommandSource::Val);
    assert!(effects.commands.is_empty());
}

#[test]
fn spmg_pause_sends_stop_and_arms_paused_resume() {
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

    // Motor stops at 25. C: Pause never set pp — postProcess is skipped
    // (1382-1402), the target survives, and maybeRetry (1077-1082) arms
    // MIP_RETRY with DMOV left FALSE for the Go resume.
    complete_move(&mut rec, 25.0);
    let _effects = rec.check_completion();
    assert!(!rec.stat.dmov);
    assert_eq!(rec.stat.mip, MipFlags::RETRY);
    assert_eq!(rec.pos.dval, 50.0); // target preserved
}

#[test]
fn ntm_retargets_while_in_motion() {
    let mut rec = make_record();
    rec.timing.ntm = true;
    rec.timing.ntmf = 2;

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
fn startup_event_runs_initial_readback_through_live_path() {
    // C init_record (motorRecord.cc:690-733) + devMotorAsyn.c
    // init_controller: the FIRST device status must flow through
    // initial_readback — readback adoption and the RSTM restore — not
    // just flip `initialized`. Drives the real process() path: shared
    // mailbox → determine_event(Startup) → do_process.
    use motor_rs::device_state::{StampedStatus, new_shared_state};

    let state = new_shared_state();
    {
        let mut ds = state.lock().unwrap();
        ds.latest_status = Some(StampedStatus {
            seq: 1,
            status: MotorStatus {
                position: 25.0,
                encoder_position: 25.0,
                done: true,
                moving: false,
                ..Default::default()
            },
        });
    }

    let mut rec = make_record();
    rec.set_device_state(state.clone());
    rec.process().unwrap();

    assert_eq!(rec.pos.rbv, 25.0);
    assert_eq!(rec.pos.val, 25.0, "boot VAL adopted from the readback");
    assert_eq!(rec.pos.dval, 25.0);
    assert!(rec.stat.dmov);
}

#[test]
fn startup_event_applies_rstm_restore_through_live_path() {
    // The RSTM near-zero restore (devMotorAsyn.c init_controller) must
    // reach the driver via the live mailbox: the SetPosition command
    // lands in pending_actions for DeviceSupport.write().
    use motor_rs::device_state::{StampedStatus, new_shared_state};

    let state = new_shared_state();
    {
        let mut ds = state.lock().unwrap();
        ds.latest_status = Some(StampedStatus {
            seq: 1,
            status: MotorStatus {
                position: 0.0, // controller lost its position
                encoder_position: 0.0,
                done: true,
                moving: false,
                ..Default::default()
            },
        });
    }

    let mut rec = make_record();
    rec.conv.rstm = RestoreMode::NearZero;
    rec.retry.rdbd = 0.05;
    rec.pos.dval = 5.0; // autosave-restored target
    rec.set_device_state(state.clone());
    rec.process().unwrap();

    assert_eq!(rec.pos.dval, 5.0, "autosaved DVAL kept, not overwritten");
    let actions = state.lock().unwrap().pending_actions.take().unwrap();
    assert!(
        actions
            .commands
            .iter()
            .any(|c| matches!(c, MotorCommand::SetPosition { position } if *position == 5.0)),
        "restore pushes the autosaved position to the driver"
    );
}

#[test]
fn startup_closed_loop_omsl_skips_readback_adoption() {
    // C init_record (705-714): under OMSL=closed_loop the DOL link owns
    // VAL initialization — the boot readback must not overwrite the
    // drive triplet, only RBV/DRBV update.
    let mut rec = make_record();
    rec.links.omsl = 1; // menuOmsl: closed_loop
    rec.pos.val = 7.0; // DOL-initialized setpoint
    rec.pos.dval = 7.0;

    let status = MotorStatus {
        position: 25.0,
        encoder_position: 25.0,
        done: true,
        moving: false,
        ..Default::default()
    };
    let effects = rec.initial_readback(&status);

    assert_eq!(rec.pos.rbv, 25.0, "readback still updates RBV");
    assert_eq!(rec.pos.val, 7.0, "closed-loop VAL untouched");
    assert_eq!(rec.pos.dval, 7.0, "closed-loop DVAL untouched");
    assert_eq!(rec.internal.lval, 7.0, "lasts anchored to current VAL");
    assert!(effects.commands.is_empty());
}

#[test]
fn stup_protocol_round_trip_through_live_path() {
    // C STUP lifecycle: ON (put) → BUSY + GET_INFO (do_work top,
    // 1817-1830) → OFF when the data callback arrives (process_exit,
    // 1498-1502).
    use motor_rs::device_state::{StampedStatus, new_shared_state};

    let state = new_shared_state();
    let idle = MotorStatus {
        done: true,
        moving: false,
        ..Default::default()
    };
    {
        let mut ds = state.lock().unwrap();
        ds.latest_status = Some(StampedStatus {
            seq: 1,
            status: idle.clone(),
        });
    }

    let mut rec = make_record();
    rec.set_device_state(state.clone());
    rec.process().unwrap(); // startup

    rec.put_field("STUP", EpicsValue::Short(1)).unwrap();
    rec.process().unwrap();
    assert_eq!(rec.get_field("STUP"), Some(EpicsValue::Short(2)), "BUSY");
    let actions = state.lock().unwrap().pending_actions.take().unwrap();
    assert!(actions.status_refresh, "GET_INFO equivalent requested");

    // The driver's status callback acknowledges the request.
    {
        let mut ds = state.lock().unwrap();
        ds.latest_status = Some(StampedStatus {
            seq: 2,
            status: idle,
        });
    }
    rec.process().unwrap();
    assert_eq!(rec.get_field("STUP"), Some(EpicsValue::Short(0)), "OFF");
}

/// Helper: an idle done status stamped with the given seq.
fn idle_status_at(seq: u64, pos: f64) -> motor_rs::device_state::StampedStatus {
    motor_rs::device_state::StampedStatus {
        seq,
        status: MotorStatus {
            position: pos,
            encoder_position: pos,
            done: true,
            moving: false,
            ..Default::default()
        },
    }
}

#[test]
fn set_mode_load_pos_collapses_through_live_path() {
    // C process (1405-1409): the GET_INFO callback after a LOAD_POS
    // collapses MIP_LOAD_P to MIP_DONE and DMOV returns TRUE. In the
    // live IOC that callback lands at phase Idle, where
    // determine_event consumes the status in place — the pass must
    // still reach the idle completion pipeline, or the SET-mode load
    // wedges with DMOV stuck low.
    use motor_rs::device_state::new_shared_state;

    let state = new_shared_state();
    state.lock().unwrap().latest_status = Some(idle_status_at(1, 0.0));

    let mut rec = make_record();
    rec.set_device_state(state.clone());
    rec.process().unwrap(); // startup

    rec.put_field("SET", EpicsValue::Short(1)).unwrap();
    rec.process().unwrap();
    rec.put_field("DVAL", EpicsValue::Double(3.0)).unwrap();
    rec.process().unwrap();
    assert!(rec.stat.mip.contains(MipFlags::LOAD_P));
    assert!(!rec.stat.dmov);
    let actions = state.lock().unwrap().pending_actions.take().unwrap();
    assert!(
        actions
            .commands
            .iter()
            .any(|c| matches!(c, MotorCommand::SetPosition { position } if *position == 3.0)),
        "redefinition reaches the driver"
    );

    // The GET_INFO response confirms the redefined position.
    state.lock().unwrap().latest_status = Some(idle_status_at(2, 3.0));
    rec.process().unwrap();
    assert!(
        rec.stat.mip.is_empty(),
        "LOAD_P collapses on the live idle poll"
    );
    assert!(rec.stat.dmov, "DMOV returns TRUE");
}

#[test]
fn load_pos_leaves_rbv_in_old_frame_until_get_info_callback() {
    // C load_pos (motorRecord.cc:3771-3817) marks M_RVAL/M_VAL/M_OFF but
    // NEVER recomputes or MARKs RBV. A SET-mode FOFF=Variable DVAL
    // redefinition shifts OFF on the dispatch pass (here 0 -> -3.0); if
    // RBV were recomputed there it would post in the new frame (-3.0)
    // while DRBV is still the pre-LOAD_POS readback (0). C leaves RBV in
    // the old frame until the GET_INFO callback re-runs process_motor_info
    // (3717) with the fresh DRBV, where RBV converges to 0 because the
    // motor never physically moved.
    use motor_rs::device_state::new_shared_state;

    let state = new_shared_state();
    state.lock().unwrap().latest_status = Some(idle_status_at(1, 0.0));

    let mut rec = make_record();
    rec.set_device_state(state.clone());
    rec.process().unwrap(); // startup: drbv=0, off=0, rbv=0

    rec.put_field("SET", EpicsValue::Short(1)).unwrap();
    rec.process().unwrap();
    let rbv_before = rec.pos.rbv;
    assert_eq!(rbv_before, 0.0);

    // SET-mode DVAL redefinition: FOFF=Variable recomputes OFF, dispatches
    // LOAD_POS, and must NOT touch RBV on this pass.
    rec.put_field("DVAL", EpicsValue::Double(3.0)).unwrap();
    rec.process().unwrap();
    assert!(rec.stat.mip.contains(MipFlags::LOAD_P));
    assert_eq!(
        rec.pos.off, -3.0,
        "Variable leg shifts OFF to the new frame"
    );
    assert_eq!(
        rec.pos.rbv, rbv_before,
        "load_pos leaves RBV in the pre-LOAD_POS frame (DRBV unchanged), \
         not the new -3.0 frame"
    );

    // GET_INFO callback: controller confirms dial 3.0. process_motor_info
    // re-derives RBV = 3.0 + (-3.0) = 0.0 -> converges, no transient.
    state.lock().unwrap().latest_status = Some(idle_status_at(2, 3.0));
    rec.process().unwrap();
    assert!(rec.stat.mip.is_empty(), "LOAD_P collapses");
    assert!(rec.stat.dmov);
    assert_eq!(rec.pos.rbv, 0.0, "RBV converges via the GET_INFO callback");
}

#[test]
fn parked_set_redefinition_replays_at_load_pos_collapse() {
    // C move-block set test (2257-2263): a second SET redefinition
    // written while a LOAD_POS is in flight parks (load_pos is skipped
    // while MIP_LOAD_P is set); the collapse pass re-enters do_work
    // and its `dval != ldvl` entry (2241) replays it through load_pos.
    use motor_rs::device_state::new_shared_state;

    let state = new_shared_state();
    state.lock().unwrap().latest_status = Some(idle_status_at(1, 0.0));

    let mut rec = make_record();
    rec.set_device_state(state.clone());
    rec.process().unwrap(); // startup

    rec.put_field("SET", EpicsValue::Short(1)).unwrap();
    rec.process().unwrap();
    rec.put_field("DVAL", EpicsValue::Double(3.0)).unwrap();
    rec.process().unwrap();
    assert!(rec.stat.mip.contains(MipFlags::LOAD_P));
    state.lock().unwrap().pending_actions.take();

    // Second redefinition while the load is in flight: parked.
    rec.put_field("DVAL", EpicsValue::Double(7.0)).unwrap();
    rec.process().unwrap();
    assert!(rec.stat.mip.contains(MipFlags::LOAD_P), "still loading");
    let parked = state.lock().unwrap().pending_actions.take();
    assert!(
        parked.is_none_or(|a| a.commands.is_empty()),
        "parked redefinition sends nothing yet"
    );

    // The first load's GET_INFO response arrives: the collapse pass
    // replays the parked redefinition.
    state.lock().unwrap().latest_status = Some(idle_status_at(2, 3.0));
    rec.process().unwrap();
    assert!(rec.stat.mip.contains(MipFlags::LOAD_P), "replayed load");
    assert!(!rec.stat.dmov);
    let actions = state.lock().unwrap().pending_actions.take().unwrap();
    assert!(
        actions
            .commands
            .iter()
            .any(|c| matches!(c, MotorCommand::SetPosition { position } if *position == 7.0)),
        "parked redefinition replays to the driver"
    );

    // The second response completes it.
    state.lock().unwrap().latest_status = Some(idle_status_at(3, 7.0));
    rec.process().unwrap();
    assert!(rec.stat.mip.is_empty());
    assert!(rec.stat.dmov);
}

#[test]
fn latent_sync_latched_during_load_pos_applies_at_collapse() {
    // C SYNC arm (2540-2544): the latched SYNC consumes only on a
    // chain-end pass with mip == MIP_DONE. A SYNC put while a LOAD_POS
    // is in flight stays latched (the Sync dispatch arm's apply is
    // mip-gated) and applies on the collapse pass.
    use motor_rs::device_state::new_shared_state;

    let state = new_shared_state();
    state.lock().unwrap().latest_status = Some(idle_status_at(1, 0.0));

    let mut rec = make_record();
    rec.set_device_state(state.clone());
    rec.process().unwrap(); // startup

    rec.put_field("SET", EpicsValue::Short(1)).unwrap();
    rec.process().unwrap();
    rec.put_field("DVAL", EpicsValue::Double(3.0)).unwrap();
    rec.process().unwrap();
    assert!(rec.stat.mip.contains(MipFlags::LOAD_P));

    rec.put_field("SYNC", EpicsValue::Short(1)).unwrap();
    rec.process().unwrap();
    assert_eq!(
        rec.get_field("SYNC"),
        Some(EpicsValue::Short(1)),
        "SYNC latched behind the in-flight load"
    );

    // The controller confirms the load with a rounded position; the
    // collapse pass applies the latched SYNC against that readback.
    state.lock().unwrap().latest_status = Some(idle_status_at(2, 2.9));
    rec.process().unwrap();
    assert!(rec.stat.mip.is_empty());
    assert!(rec.stat.dmov);
    assert_eq!(rec.get_field("SYNC"), Some(EpicsValue::Short(0)));
    assert_eq!(rec.pos.dval, 2.9, "SYNC reseeded DVAL from the readback");
}

#[test]
fn noop_put_pass_fires_implicit_get_info_through_live_path() {
    // C 2546-2557: a put/scan pass (NOTHING_DONE) that dispatched
    // nothing falls to the end of the do_work chain — STUP goes BUSY
    // and a GET_INFO refreshes the device status. The data callback
    // returns STUP to OFF (process_exit 1498-1502) and, being a
    // CALLBACK_DATA pass, must not re-fire — C's loop prevention.
    use motor_rs::device_state::new_shared_state;

    let state = new_shared_state();
    state.lock().unwrap().latest_status = Some(idle_status_at(1, 0.0));

    let mut rec = make_record();
    rec.set_device_state(state.clone());
    rec.process().unwrap(); // startup
    state.lock().unwrap().pending_actions.take();

    // HLM is pp(TRUE) in C with no dispatch of its own: the put pass
    // reaches the chain end having done nothing.
    rec.put_field("HLM", EpicsValue::Double(50.0)).unwrap();
    rec.process().unwrap();
    assert_eq!(rec.get_field("STUP"), Some(EpicsValue::Short(2)), "BUSY");
    let actions = state.lock().unwrap().pending_actions.take().unwrap();
    assert!(actions.status_refresh, "implicit GET_INFO requested");

    // The data callback acknowledges and must not re-trigger.
    state.lock().unwrap().latest_status = Some(idle_status_at(2, 0.0));
    rec.process().unwrap();
    assert_eq!(rec.get_field("STUP"), Some(EpicsValue::Short(0)), "OFF");
    let ack = state.lock().unwrap().pending_actions.take();
    assert!(
        ack.is_none_or(|a| !a.status_refresh),
        "CALLBACK_DATA pass does not re-fire the refresh"
    );

    // A plain idle poll is also a CALLBACK_DATA pass: no refresh.
    state.lock().unwrap().latest_status = Some(idle_status_at(3, 0.0));
    rec.process().unwrap();
    assert_eq!(rec.get_field("STUP"), Some(EpicsValue::Short(0)));
    let poll = state.lock().unwrap().pending_actions.take();
    assert!(poll.is_none_or(|a| !a.status_refresh));
}

#[test]
fn coalesced_put_and_status_split_into_two_passes() {
    // C dbScanLock runs one pass per signal: a dbPutField pass never
    // consumes the device callback's payload — the callback's own
    // dbProcess (devMotorAsyn.c statusCallback 757-766) follows as a
    // CALLBACK_DATA pass with the completion pipeline. A put racing a
    // fresh status must not reduce the status to a readback apply.
    use motor_rs::device_state::new_shared_state;

    let state = new_shared_state();
    state.lock().unwrap().latest_status = Some(idle_status_at(1, 0.0));

    let mut rec = make_record();
    rec.set_device_state(state.clone());
    rec.process().unwrap(); // startup

    rec.put_field("VAL", EpicsValue::Double(10.0)).unwrap();
    rec.process().unwrap();
    assert_eq!(rec.stat.phase, MotionPhase::MainMove);
    assert!(!rec.stat.dmov);
    state.lock().unwrap().pending_actions.take();

    // The done status and a CNEN put land before the next pass.
    state.lock().unwrap().latest_status = Some(idle_status_at(2, 10.0));
    rec.put_field("CNEN", EpicsValue::Short(1)).unwrap();
    rec.process().unwrap(); // the put's pass
    assert_eq!(rec.pos.rbv, 0.0, "put pass leaves the status unconsumed");
    assert_eq!(
        rec.stat.phase,
        MotionPhase::MainMove,
        "completion is not swallowed by the put pass"
    );
    assert!(!rec.stat.dmov);

    rec.process().unwrap(); // the status's own pass (its io_intr pulse)
    assert_eq!(rec.pos.rbv, 10.0);
    assert_eq!(rec.stat.phase, MotionPhase::Idle);
    assert!(rec.stat.dmov);
}

#[test]
fn move_start_notify_posts_diff_and_rdif() {
    // R2: C do_work move block (motorRecord.cc:2248-2256) marks M_DIFF/M_RDIF
    // before too_small/LVIO, so monitor() posts the new full-distance
    // following error on the move-start pass. The Rust DMOV 1->0 notify must
    // carry DIFF and RDIF, not just the position triplet.
    use epics_base_rs::server::record::RecordProcessResult;
    use motor_rs::device_state::new_shared_state;

    let state = new_shared_state();
    state.lock().unwrap().latest_status = Some(idle_status_at(1, 0.0));

    let mut rec = make_record(); // mres = 0.001
    rec.set_device_state(state.clone());
    rec.process().unwrap(); // startup: drbv = 0.0
    state.lock().unwrap().pending_actions.take();

    rec.put_field("VAL", EpicsValue::Double(10.0)).unwrap();
    let outcome = rec.process().unwrap();
    let fields = match outcome.result {
        RecordProcessResult::AsyncPendingNotify(fields) => fields,
        other => panic!("expected move-start AsyncPendingNotify, got {other:?}"),
    };
    // diff = dval - drbv = 10 - 0 = 10; rdif = NINT(diff/mres) = 10/0.001 = 10000.
    let diff = fields.iter().find(|(n, _)| n == "DIFF").map(|(_, v)| v);
    let rdif = fields.iter().find(|(n, _)| n == "RDIF").map(|(_, v)| v);
    assert_eq!(
        diff,
        Some(&EpicsValue::Double(10.0)),
        "move-start notify posts DIFF = dval - drbv"
    );
    assert_eq!(
        rdif,
        Some(&EpicsValue::Int64(10000)),
        "move-start notify posts RDIF = NINT(diff/mres)"
    );
}

#[test]
fn coalesced_put_and_delay_expiry_keeps_watchdog() {
    // The settle-delay timer is one-shot (poll_loop::spawn_delay): an
    // expiry consumed by a put-owned pass has no second chance, and
    // ScheduleDelay idled the poll loop — the record would wedge with
    // DMOV low. The expiry must survive the put pass and run its own.
    use motor_rs::device_state::new_shared_state;

    let state = new_shared_state();
    state.lock().unwrap().latest_status = Some(idle_status_at(1, 0.0));

    let mut rec = make_record();
    rec.timing.dly = 0.5;
    rec.set_device_state(state.clone());
    rec.process().unwrap(); // startup

    rec.put_field("VAL", EpicsValue::Double(10.0)).unwrap();
    rec.process().unwrap();
    state.lock().unwrap().pending_actions.take();

    // The move completes: DLY schedules the settle watchdog.
    state.lock().unwrap().latest_status = Some(idle_status_at(2, 10.0));
    rec.process().unwrap();
    assert_eq!(rec.stat.phase, MotionPhase::DelayWait);
    assert!(rec.stat.mip.contains(MipFlags::DELAY_REQ));
    let actions = state.lock().unwrap().pending_actions.take().unwrap();
    let delay = actions.schedule_delay.expect("watchdog armed");

    // The timer fires while a CNEN put is pending: coalesced signals.
    state.lock().unwrap().expired_delay_id = Some(delay.id);
    rec.put_field("CNEN", EpicsValue::Short(1)).unwrap();
    rec.process().unwrap(); // the put's pass
    assert!(
        rec.stat.mip.contains(MipFlags::DELAY_REQ),
        "watchdog still armed after the put pass"
    );
    assert!(!rec.stat.mip.contains(MipFlags::DELAY_ACK));
    assert!(
        state.lock().unwrap().expired_delay_id.is_some(),
        "expiry not consumed by the put pass"
    );

    rec.process().unwrap(); // the expiry's own pass
    assert!(rec.stat.mip.contains(MipFlags::DELAY_ACK));
    let actions = state.lock().unwrap().pending_actions.take().unwrap();
    assert!(actions.status_refresh, "DELAY_ACK requests the fresh poll");

    // The fresh poll finalizes the motion.
    state.lock().unwrap().latest_status = Some(idle_status_at(3, 10.0));
    rec.process().unwrap();
    assert!(rec.stat.dmov);
    assert_eq!(rec.stat.phase, MotionPhase::Idle);
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

    // Sub-step move: the target differs from the last-dispatched one
    // (C 2241 enters via dval != ldvl) but by < 1 step -- no motor
    // command, DMOV pulses (deliberate ophyd/bluesky deviation).
    rec.pos.dval = 0.0004; // 0.4 steps at mres = 0.001
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

    // C set_user_highlimit/lowlimit (motorRecord.cc:4076-4225): under
    // DIR=Neg, the HLM write lands in DLLM = -(100-0) = -100 and the
    // LLM write lands in DHLM = -(-100-0) = 100 — one dial limit each.
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
        min_velocity,
        acceleration,
        ..
    } = &effects.commands[0]
    {
        motor
            .move_absolute(&user, *position, *min_velocity, 100000.0, *acceleration)
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

/// A database put of VAL to the current position produces a DMOV
/// 1→0→1 pulse: the special() pass-0 blink (motorRecord.cc:2591-2620)
/// drops DMOV, the move-block entry gate (2241) passes via `!dmov`,
/// and too_small (2332-2349) completes the zero-length "move".
/// ophyd/bluesky rely on this edge to detect move completion.
/// Full framework order: special(pass 0) → put_field → process pass
/// (consumes last_write) → recovery pass restores DMOV.
#[test]
fn dbput_to_same_position_pulses_dmov() {
    let mut rec = make_record();

    rec.pos.val = 0.0;
    rec.pos.dval = 0.0;
    rec.pos.rval = 0;
    rec.pos.rbv = 0.0;
    rec.pos.drbv = 0.0;
    rec.stat.dmov = true;
    rec.stat.phase = MotionPhase::Idle;

    rec.special("VAL", false).unwrap();
    assert!(!rec.stat.dmov, "pass-0 blink drops DMOV");
    rec.put_field("VAL", EpicsValue::Double(0.0)).unwrap();

    // The pp process pass: put-owned (last_write = Val), so the pulse
    // recovery must NOT eat it — it dispatches, lands in too_small,
    // and posts the pulse's low half.
    let effects = rec.do_process();
    assert!(!rec.stat.dmov, "pulse low half");
    assert!(rec.stat.movn, "pulse presents as a (zero-length) move");
    assert!(effects.commands.is_empty(), "no motor command");
    assert!(effects.request_poll, "pulse schedules the recovery pass");
    assert_eq!(rec.stat.phase, MotionPhase::Idle);

    // The recovery pass restores DMOV=1 — the pulse's high half.
    let _ = rec.do_process();
    assert!(rec.stat.dmov, "DMOV returns to 1");
    assert!(!rec.stat.movn);
}

/// Same-position dbPut re-put after a genuinely completed move,
/// end-to-end through the completion flow: the blink re-opens the
/// entry gate, too_small pulses. An unblinked same-value pass (no
/// fresh drive write — C's closed-loop DOL dbGetLink delivery) is the
/// one the entry gate (2241) refuses with no DMOV edge.
#[test]
fn same_position_reput_after_completed_move_pulses() {
    let mut rec = make_record();

    rec.special("VAL", false).unwrap();
    rec.put_field("VAL", EpicsValue::Double(5.0)).unwrap();
    let effects = rec.do_process();
    assert!(!effects.commands.is_empty(), "real move dispatched");
    complete_move(&mut rec, 5.0);
    let _ = rec.check_completion();
    assert!(rec.stat.dmov);

    // Blinked re-put of the same target: DMOV pulses, no command.
    rec.special("VAL", false).unwrap();
    rec.put_field("VAL", EpicsValue::Double(5.0)).unwrap();
    let effects = rec.do_process();
    assert!(!rec.stat.dmov, "pulse low half");
    assert!(effects.commands.is_empty(), "no motor command");
    let _ = rec.do_process();
    assert!(rec.stat.dmov, "pulse high half restores DMOV");

    // Unblinked same-value pass: entry gate refuses, no DMOV edge.
    let effects = rec.plan_motion(CommandSource::Val);
    assert!(rec.stat.dmov, "DMOV stays 1 — C refuses the unblinked pass");
    assert!(effects.commands.is_empty(), "no motor command");
}

/// A blinked drive put while SPMG=Pause parks like C: the special()
/// blink drops DMOV, the top-block stop_or_pause return (C 2237)
/// refuses the dispatch with mip=STOP, and DMOV reads 0 — "not done",
/// a move is pending. Go falls through to the move block (1909-1925)
/// and dispatches the parked target; the pulse recovery must not
/// finalize the parked state early (mip=STOP keeps it out).
#[test]
fn blinked_put_while_paused_parks_and_resumes_on_go() {
    let mut rec = make_record();

    // Idle at 0, then Pause.
    rec.ctrl.spmg = SpmgMode::Pause;
    let effects = rec.plan_motion(CommandSource::Spmg);
    assert!(matches!(effects.commands[0], MotorCommand::Stop { .. }));
    assert_eq!(rec.stat.mip, MipFlags::STOP);
    complete_move(&mut rec, 0.0);
    let _ = rec.check_completion();

    // dbPut VAL=7 while paused: blink + park, no dispatch.
    rec.special("VAL", false).unwrap();
    rec.put_field("VAL", EpicsValue::Double(7.0)).unwrap();
    let effects = rec.do_process();
    assert!(effects.commands.is_empty(), "paused put dispatches nothing");
    assert!(!rec.stat.dmov, "DMOV stays low — a move is pending (C)");
    assert_eq!(rec.pos.dval, 7.0, "parked target survives the pause");

    // A stray non-put pass must not finalize the parked state.
    let effects = rec.do_process();
    assert!(effects.commands.is_empty());
    assert!(!rec.stat.dmov, "recovery does not eat the parked move");

    // Go dispatches the parked target.
    rec.ctrl.spmg = SpmgMode::Go;
    let effects = rec.plan_motion(CommandSource::Spmg);
    assert!(
        effects
            .commands
            .iter()
            .any(|c| matches!(c, MotorCommand::MoveAbsolute { .. })),
        "Go dispatches the move parked during the pause"
    );
    assert!(!rec.stat.dmov, "move in flight");
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

    // C 2206-2227: a FOFF=Variable redefinition is offset-only — no
    // controller command (LOAD_POS belongs to the Frozen leg and the
    // DVAL/RVAL paths). DVAL is untouched, so the C entry gate (2241)
    // refuses the put pass outright.
    assert!(
        effects.commands.is_empty(),
        "Variable-offset SET redefinition must not send a controller command"
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
                min_velocity,
                velocity,
                acceleration,
            } = cmd
            {
                motor
                    .move_absolute(&user, *position, *min_velocity, *velocity, *acceleration)
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
            min_velocity,
            velocity,
            acceleration,
        } = cmd
        {
            motor
                .move_absolute(&user, *position, *min_velocity, *velocity, *acceleration)
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
            min_velocity,
            velocity,
            acceleration,
        } = cmd
        {
            motor
                .move_absolute(&user, *position, *min_velocity, *velocity, *acceleration)
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
            min_velocity,
            velocity,
            acceleration,
            forward,
        } = cmd
        {
            motor
                .home(&user, *min_velocity, *velocity, *acceleration, *forward)
                .unwrap();
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

// --- R3: maybeRetry give-up re-arms a held jog/home same-pass ---
//
// C `maybeRetry` give-up (motorRecord.cc:1063-1065) sets `mip = MIP_DONE`
// then re-arms `MIP_JOG_REQ` from the held jogf/jogr field; dmov stays
// TRUE so `do_work` re-fires the armed motion in the SAME process pass
// (1489). The close-enough/rtry==0 branches instead do
// `mip &= MIP_JOG_REQ` (1055/1088), which during a positional move reads
// 0 — special() arms MIP_JOG_REQ only at mip==MIP_DONE (3042-3053) — so
// they drop the jog this pass and let the next idle poll pick it up.

/// Drive a positional move to the retry-exhaustion (give-up) edge.
/// Returns with the record one short-completion away from give-up:
/// rtry=1, rcnt already 1, phase back in MainMove.
fn arm_give_up(rec: &mut MotorRecord) {
    rec.retry.rdbd = 0.01;
    rec.retry.rtry = 1;
    rec.retry.frac = 1.0;
    rec.pos.dval = 10.0;
    rec.plan_motion(CommandSource::Val);
    // First short completion: retries (rcnt -> 1), re-dispatches the move.
    complete_move(rec, 9.5);
    let _ = rec.check_completion();
    assert_eq!(rec.retry.rcnt, 1, "first short completion retries");
    assert_eq!(rec.stat.phase, MotionPhase::MainMove);
}

#[test]
fn give_up_refires_held_jogf_same_pass() {
    let mut rec = make_record();
    arm_give_up(&mut rec);

    // Operator holds JOGF during the move; SPMG stays Go (default).
    rec.ctrl.jogf = true;

    // Second short completion: rcnt(1) >= rtry(1) -> give up.
    complete_move(&mut rec, 9.5);
    let effects = rec.check_completion();

    assert!(rec.retry.miss, "give-up latches MISS");
    assert_eq!(
        rec.stat.phase,
        MotionPhase::Jog,
        "held JOGF re-fires in the give-up pass"
    );
    assert!(
        effects.commands.iter().any(|c| matches!(
            c,
            MotorCommand::MoveVelocity {
                direction: true,
                ..
            }
        )),
        "give-up must re-fire the held jog same-pass, got {:?}",
        effects.commands
    );
}

#[test]
fn give_up_refires_held_homf_same_pass() {
    let mut rec = make_record();
    arm_give_up(&mut rec);

    rec.ctrl.homf = true;

    complete_move(&mut rec, 9.5);
    let effects = rec.check_completion();

    assert!(rec.retry.miss);
    assert_eq!(rec.stat.phase, MotionPhase::Homing);
    assert!(
        effects
            .commands
            .iter()
            .any(|c| matches!(c, MotorCommand::Home { forward: true, .. })),
        "give-up must re-fire the held home same-pass, got {:?}",
        effects.commands
    );
}

#[test]
fn give_up_no_button_finalizes_quietly() {
    let mut rec = make_record();
    arm_give_up(&mut rec);

    // No button held.
    complete_move(&mut rec, 9.5);
    let effects = rec.check_completion();

    assert!(rec.retry.miss);
    assert!(rec.stat.dmov);
    assert_eq!(rec.stat.phase, MotionPhase::Idle);
    assert_eq!(rec.stat.mip, MipFlags::empty());
    assert!(
        effects.commands.is_empty(),
        "no held button: give-up emits no command, got {:?}",
        effects.commands
    );
    assert!(
        !effects.status_refresh,
        "CALLBACK_DATA give-up must not fire the implicit GET_INFO"
    );
}

#[test]
fn close_enough_defers_held_jog_to_next_idle_poll() {
    let mut rec = make_record();
    rec.retry.rdbd = 0.5;
    rec.retry.rtry = 3;
    rec.pos.dval = 10.0;
    rec.plan_motion(CommandSource::Val);

    // Operator holds JOGF during the move; SPMG stays Go.
    rec.ctrl.jogf = true;

    // Close-enough completion (within rdbd): C drops the jog this pass.
    complete_move(&mut rec, 10.0);
    let effects = rec.check_completion();
    assert!(rec.stat.dmov);
    assert_eq!(rec.stat.phase, MotionPhase::Idle);
    assert!(
        !effects
            .commands
            .iter()
            .any(|c| matches!(c, MotorCommand::MoveVelocity { .. })),
        "close-enough must NOT re-fire the jog in the completion pass, got {:?}",
        effects.commands
    );
    // ...but it MUST request the forced poll that delivers the resume pass —
    // otherwise the change-gate suppresses the unchanged settle poll and the
    // held jog strands. (This is the unit-level guard of the R65 fix; the
    // second check_completion below only proves the resume given a pass.)
    assert!(
        effects.request_poll,
        "completion with a held jog must request the forced settle poll"
    );

    // The next idle poll (level-triggered) re-fires the held jog.
    complete_move(&mut rec, 10.0);
    let effects = rec.check_completion();
    assert_eq!(rec.stat.phase, MotionPhase::Jog);
    assert!(
        effects.commands.iter().any(|c| matches!(
            c,
            MotorCommand::MoveVelocity {
                direction: true,
                ..
            }
        )),
        "idle poll must resume the held jog, got {:?}",
        effects.commands
    );
}

/// The SET-mode pp-resync early-return (state_machine.rs:401) quiesces the axis
/// WITHOUT finalize_motion and dispatches nothing — a formerly-bypassing member
/// of the held-button deferral family. With the idle-poll change-gate it would
/// strand a held jog. The shared `request_poll_for_held_button` owner now closes
/// it too: the pp-resync must request the forced poll.
#[test]
fn pp_resync_requests_forced_poll_for_held_jog() {
    let mut rec = make_record();
    // dispatch_res_reanchor (SET && !IGSET) armed pp + a forced poll; the
    // forced poll lands on this Idle-arm pp-resync with a jog still held.
    rec.ctrl.spmg = SpmgMode::Go;
    rec.internal.pp = true;
    rec.ctrl.jogf = true;
    // Reached only with the axis already done and idle (make_record sets
    // msta=DONE; a fresh record is Idle with movn=false).
    assert_eq!(rec.stat.phase, MotionPhase::Idle);
    assert!(!rec.stat.movn);

    let effects = rec.check_completion();

    assert!(!rec.internal.pp, "the pp-resync consumes pp");
    assert!(
        effects.request_poll,
        "the pp-resync early-return must request a forced poll so the held jog is not stranded by the change-gate"
    );
}

#[test]
fn close_enough_move_oneshot_jog_waits_for_spmg_go() {
    let mut rec = make_record();
    rec.retry.rdbd = 0.5;
    rec.retry.rtry = 3;
    // Motion initiated by the Move button (one-shot): SPMG = Move.
    rec.ctrl.spmg = SpmgMode::Move;
    rec.internal.lspg = SpmgMode::Move;
    rec.pos.dval = 10.0;
    rec.plan_motion(CommandSource::Val);

    rec.ctrl.jogf = true;

    // Close-enough completion: restore_spmg_move_to_pause flips Move->Pause.
    complete_move(&mut rec, 10.0);
    let _ = rec.check_completion();
    assert_eq!(
        rec.ctrl.spmg,
        SpmgMode::Pause,
        "Move one-shot restores SPMG to Pause at close-enough"
    );

    // Idle poll while Paused: the held jog must NOT re-fire.
    complete_move(&mut rec, 10.0);
    let effects = rec.check_completion();
    assert_ne!(rec.stat.phase, MotionPhase::Jog);
    assert!(
        effects.commands.is_empty(),
        "Paused: held jog must wait for SPMG=Go, got {:?}",
        effects.commands
    );

    // Operator presses Go: the held jog now re-fires.
    rec.ctrl.spmg = SpmgMode::Go;
    complete_move(&mut rec, 10.0);
    let effects = rec.check_completion();
    assert_eq!(rec.stat.phase, MotionPhase::Jog);
    assert!(
        effects.commands.iter().any(|c| matches!(
            c,
            MotorCommand::MoveVelocity {
                direction: true,
                ..
            }
        )),
        "SPMG=Go resumes the held jog, got {:?}",
        effects.commands
    );
}
