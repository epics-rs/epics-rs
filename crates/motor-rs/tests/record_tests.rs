use epics_base_rs::server::record::Record;
use epics_base_rs::types::EpicsValue;
use motor_rs::MotorRecord;
use motor_rs::flags::*;

#[test]
fn test_default_values() {
    let rec = MotorRecord::new();
    assert_eq!(rec.pos.val, 0.0);
    assert!(rec.stat.dmov);
    assert!(!rec.stat.movn);
    assert_eq!(rec.stat.phase, MotionPhase::Idle);
    assert_eq!(rec.stat.mip, MipFlags::empty());
    assert_eq!(rec.ctrl.spmg, SpmgMode::Go);
    assert_eq!(rec.conv.mres, 1.0);
    assert_eq!(rec.vel.velo, 1.0);
    assert_eq!(rec.vel.accl, 0.2);
    assert_eq!(rec.retry.rtry, 10);
    assert!(rec.limits.lvio); // default true (no limits set)
}

#[test]
fn test_record_type() {
    let rec = MotorRecord::new();
    assert_eq!(rec.record_type(), "motor");
}

#[test]
fn test_field_roundtrip_double() {
    let mut rec = MotorRecord::new();
    rec.put_field("VAL", EpicsValue::Double(42.0)).unwrap();
    assert_eq!(rec.get_field("VAL"), Some(EpicsValue::Double(42.0)));
}

#[test]
fn test_field_roundtrip_short() {
    let mut rec = MotorRecord::new();
    rec.put_field("PREC", EpicsValue::Short(3)).unwrap();
    assert_eq!(rec.get_field("PREC"), Some(EpicsValue::Short(3)));
}

#[test]
fn test_field_roundtrip_string() {
    let mut rec = MotorRecord::new();
    rec.put_field("EGU", EpicsValue::String("mm".into()))
        .unwrap();
    assert_eq!(rec.get_field("EGU"), Some(EpicsValue::String("mm".into())));
}

#[test]
fn test_val_cascades_to_dval_rval() {
    let mut rec = MotorRecord::new();
    rec.conv.mres = 0.01;
    rec.put_field("VAL", EpicsValue::Double(10.0)).unwrap();
    assert_eq!(rec.pos.dval, 10.0);
    assert_eq!(rec.pos.rval, 1000);
}

#[test]
fn test_dval_cascades_to_val_rval() {
    let mut rec = MotorRecord::new();
    rec.conv.mres = 0.01;
    rec.put_field("DVAL", EpicsValue::Double(5.0)).unwrap();
    assert_eq!(rec.pos.val, 5.0);
    assert_eq!(rec.pos.rval, 500);
}

#[test]
fn test_rval_cascades_to_val_dval() {
    let mut rec = MotorRecord::new();
    rec.conv.mres = 0.01;
    rec.put_field("RVAL", EpicsValue::Long(1000)).unwrap();
    assert_eq!(rec.pos.dval, 10.0);
    assert_eq!(rec.pos.val, 10.0);
}

#[test]
fn test_type_mismatch() {
    let mut rec = MotorRecord::new();
    let result = rec.put_field("VAL", EpicsValue::String("bad".into()));
    assert!(result.is_err());
}

#[test]
fn test_unknown_field() {
    let mut rec = MotorRecord::new();
    let result = rec.put_field("NONEXIST", EpicsValue::Double(0.0));
    assert!(result.is_err());
}

#[test]
fn test_hlm_cascades_to_dhlm() {
    let mut rec = MotorRecord::new();
    rec.put_field("HLM", EpicsValue::Double(100.0)).unwrap();
    rec.put_field("LLM", EpicsValue::Double(-100.0)).unwrap();
    assert_eq!(rec.limits.dhlm, 100.0);
    assert_eq!(rec.limits.dllm, -100.0);
}

#[test]
fn test_dir_neg_limit_mapping() {
    let mut rec = MotorRecord::new();
    rec.conv.dir = MotorDir::Neg;
    rec.put_field("HLM", EpicsValue::Double(100.0)).unwrap();
    rec.put_field("LLM", EpicsValue::Double(-100.0)).unwrap();
    // DIR=Neg: user 100 -> dial -100, user -100 -> dial 100
    assert_eq!(rec.limits.dhlm, 100.0);
    assert_eq!(rec.limits.dllm, -100.0);
}

#[test]
fn test_spmg_blocks_commands() {
    let mut rec = MotorRecord::new();
    rec.ctrl.spmg = SpmgMode::Stop;
    assert!(!rec.can_accept_command());
    rec.ctrl.spmg = SpmgMode::Pause;
    assert!(!rec.can_accept_command());
    rec.ctrl.spmg = SpmgMode::Go;
    assert!(rec.can_accept_command());
    rec.ctrl.spmg = SpmgMode::Move;
    assert!(rec.can_accept_command());
}

#[test]
fn test_compute_dmov() {
    let mut rec = MotorRecord::new();
    rec.stat.msta = MstaFlags::DONE;
    rec.stat.phase = MotionPhase::Idle;
    assert!(rec.compute_dmov());

    rec.stat.msta = MstaFlags::MOVING;
    assert!(!rec.compute_dmov());

    rec.stat.msta = MstaFlags::DONE;
    rec.stat.phase = MotionPhase::MainMove;
    assert!(!rec.compute_dmov());
}

#[test]
fn test_process_motor_info() {
    let mut rec = MotorRecord::new();
    rec.conv.mres = 0.001;
    let status = asyn_rs::interfaces::motor::MotorStatus {
        position: 10.0,
        encoder_position: 10.001,
        done: true,
        moving: false,
        ..Default::default()
    };
    rec.process_motor_info(&status);
    assert_eq!(rec.pos.rmp, 10000);
    assert_eq!(rec.pos.drbv, 10.0);
    assert_eq!(rec.pos.rbv, 10.0);
    assert!(!rec.stat.movn);
    assert!(rec.stat.msta.contains(MstaFlags::DONE));
}

#[test]
fn test_sync_positions() {
    let mut rec = MotorRecord::new();
    rec.pos.drbv = 5.0;
    rec.pos.rbv = 5.0;
    rec.pos.rrbv = 500;
    rec.sync_positions();
    assert_eq!(rec.pos.dval, 5.0);
    assert_eq!(rec.pos.val, 5.0);
    assert_eq!(rec.pos.rval, 500);
    assert_eq!(rec.pos.diff, 0.0);
}

#[test]
fn test_soft_limit_rejects_move() {
    let mut rec = MotorRecord::new();
    rec.conv.mres = 0.01;
    rec.limits.dhlm = 100.0;
    rec.limits.dllm = -100.0;
    rec.limits.lvio = false;
    rec.stat.msta = MstaFlags::DONE;

    // Try to move beyond limits
    rec.pos.dval = 200.0;
    let effects = rec.plan_motion(CommandSource::Val);
    assert!(rec.limits.lvio);
    assert!(effects.commands.is_empty());
}

#[test]
fn test_absolute_move_sets_dmov_false() {
    let mut rec = MotorRecord::new();
    rec.conv.mres = 0.01;
    rec.limits.dhlm = 100.0;
    rec.limits.dllm = -100.0;
    rec.stat.msta = MstaFlags::DONE;
    rec.pos.dval = 50.0;

    let effects = rec.plan_motion(CommandSource::Val);
    assert!(!rec.stat.dmov);
    assert_eq!(rec.stat.phase, MotionPhase::MainMove);
    assert_eq!(effects.commands.len(), 1);
    assert!(effects.request_poll);
    assert!(matches!(
        effects.commands[0],
        MotorCommand::MoveAbsolute { .. }
    ));
}

#[test]
fn test_stop_during_move() {
    let mut rec = MotorRecord::new();
    rec.stat.phase = MotionPhase::MainMove;
    rec.stat.mip = MipFlags::MOVE;
    rec.stat.dmov = false;
    rec.pos.rbv = 25.0;
    rec.pos.drbv = 25.0;
    rec.pos.rrbv = 2500;

    let effects = rec.plan_motion(CommandSource::Stop);
    assert!(rec.stat.mip.contains(MipFlags::STOP));
    assert_eq!(effects.commands.len(), 1);
    assert!(matches!(effects.commands[0], MotorCommand::Stop { .. }));
    // C parity: drive fields are NOT synced at stop-put time (C only sets
    // pp=TRUE while the axis decelerates, motorRecord.cc:1891-1893)...
    assert_eq!(rec.pos.val, 0.0);

    // ...the VAL<-RBV sync happens when the axis has actually stopped.
    rec.stat.msta = MstaFlags::DONE;
    let _ = rec.check_completion();
    assert!(rec.stat.dmov);
    assert_eq!(rec.pos.val, 25.0);
}

#[test]
fn test_jog_start_stop() {
    let mut rec = MotorRecord::new();
    rec.conv.mres = 0.01;
    rec.stat.msta = MstaFlags::DONE;

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
}

#[test]
fn test_home_forward() {
    let mut rec = MotorRecord::new();
    rec.stat.msta = MstaFlags::DONE;

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
}

#[test]
fn test_tweak_forward() {
    let mut rec = MotorRecord::new();
    rec.conv.mres = 0.01;
    rec.limits.dhlm = 100.0;
    rec.limits.dllm = -100.0;
    rec.stat.msta = MstaFlags::DONE;
    rec.ctrl.twv = 5.0;
    rec.pos.val = 10.0;
    rec.pos.dval = 10.0;

    rec.ctrl.twf = true;
    let effects = rec.plan_motion(CommandSource::Twf);
    assert_eq!(rec.pos.val, 15.0); // 10 + 5
    assert!(!rec.ctrl.twf); // pulse cleared
    assert!(!effects.commands.is_empty());
}

#[test]
fn test_tweak_in_set_mode_redefines_coordinates_without_moving() {
    // C: a tweak that changes VAL in SET mode redefines coordinates (the
    // VAL-change path), it does not move the motor.
    let mut rec = MotorRecord::new();
    rec.conv.mres = 0.01;
    rec.limits.dhlm = 100.0;
    rec.limits.dllm = -100.0;
    rec.stat.msta = MstaFlags::DONE;
    rec.conv.set = true; // SET mode
    rec.ctrl.twv = 5.0;
    rec.pos.val = 10.0;
    rec.pos.dval = 10.0;
    rec.pos.off = 0.0;

    rec.ctrl.twf = true;
    let effects = rec.plan_motion(CommandSource::Twf);

    assert_eq!(rec.pos.val, 15.0);
    // SET + FOFF=Variable: DVAL unchanged, OFF re-derived (15 - 1*10 = 5).
    assert_eq!(rec.pos.dval, 10.0);
    assert_eq!(rec.pos.off, 5.0);
    // No move command — only SetPosition (coordinate redefinition).
    assert!(
        !effects.commands.iter().any(|c| matches!(
            c,
            MotorCommand::MoveAbsolute { .. } | MotorCommand::MoveRelative { .. }
        )),
        "SET-mode tweak must not move the motor"
    );
    assert!(
        effects
            .commands
            .iter()
            .any(|c| matches!(c, MotorCommand::SetPosition { .. }))
    );
}

#[test]
fn test_tweak_blocked_when_loadpos_blocked_in_set_mode() {
    let mut rec = MotorRecord::new();
    rec.conv.mres = 0.01;
    rec.stat.msta = MstaFlags::DONE;
    rec.conv.set = true;
    rec.conv.loadpos_blocked = true;
    rec.ctrl.twv = 5.0;
    rec.pos.val = 10.0;
    rec.pos.dval = 10.0;
    rec.pos.off = 0.0;

    rec.ctrl.twf = true;
    let effects = rec.plan_motion(CommandSource::Twf);
    // LOAD_POS blocked: tweak refused, VAL/DVAL/OFF unchanged.
    assert_eq!(rec.pos.val, 10.0);
    assert_eq!(rec.pos.dval, 10.0);
    assert_eq!(rec.pos.off, 0.0);
    assert!(effects.commands.is_empty());
}

#[test]
fn test_field_list_coverage() {
    let rec = MotorRecord::new();
    let fields = rec.field_list();
    // All fields in the list should be gettable
    for fd in fields {
        assert!(
            rec.get_field(fd.name).is_some(),
            "field {} not gettable",
            fd.name
        );
    }
}

#[test]
fn test_dly_delays_finalization() {
    let mut rec = MotorRecord::new();
    rec.timing.dly = 1.0;
    rec.stat.msta = MstaFlags::DONE;
    rec.stat.phase = MotionPhase::MainMove;
    rec.stat.dmov = false; // motion in progress
    rec.retry.rdbd = 0.0; // no retry

    let effects = rec.check_completion();
    assert_eq!(rec.stat.phase, MotionPhase::DelayWait);
    assert!(effects.schedule_delay.is_some());
    assert!(!rec.stat.dmov); // still false during delay
}

#[test]
fn test_retry_on_position_error() {
    let mut rec = MotorRecord::new();
    rec.stat.msta = MstaFlags::DONE;
    rec.stat.phase = MotionPhase::MainMove;
    // MRES smaller than RDBD so enforceMinRetryDeadband (run at the top
    // of check_completion, C parity with do_work) leaves RDBD untouched.
    rec.conv.mres = 0.001;
    rec.retry.rdbd = 0.1;
    rec.retry.rtry = 3;
    rec.pos.dval = 10.0;
    rec.pos.drbv = 9.5; // error = 0.5 > rdbd

    let effects = rec.check_completion();
    assert_eq!(rec.stat.phase, MotionPhase::Retry);
    assert_eq!(rec.retry.rcnt, 1);
    assert!(!effects.commands.is_empty());
}

#[test]
fn test_miss_when_retries_exhausted() {
    let mut rec = MotorRecord::new();
    rec.stat.msta = MstaFlags::DONE;
    rec.stat.phase = MotionPhase::MainMove;
    // MRES smaller than RDBD so enforceMinRetryDeadband is a no-op here.
    rec.conv.mres = 0.001;
    rec.retry.rdbd = 0.1;
    rec.retry.rtry = 3;
    rec.retry.rcnt = 3; // exhausted
    rec.pos.dval = 10.0;
    rec.pos.drbv = 9.5;

    let _effects = rec.check_completion();
    assert!(rec.retry.miss);
    // Should finalize (or delay)
    assert_eq!(rec.stat.phase, MotionPhase::Idle);
}

#[test]
fn test_ntm_retarget_direction_change() {
    let mut rec = MotorRecord::new();
    rec.timing.ntm = true;
    rec.timing.ntmf = 2.0;
    rec.retry.bdst = 0.0;
    rec.retry.rdbd = 0.0;
    rec.internal.ldvl = 10.0;
    rec.pos.drbv = 5.0;
    // Set up active motion state (required for NTM)
    rec.stat.mip = MipFlags::MOVE;
    rec.stat.phase = MotionPhase::MainMove;
    // CDIR: moving positive (dval > drbv)
    rec.stat.cdir = true;

    // Same direction, farther -> ExtendMove
    assert_eq!(rec.handle_retarget(15.0), RetargetAction::ExtendMove);

    // Opposite direction (negative, deadband=0) -> StopAndReplan
    assert_eq!(rec.handle_retarget(-5.0), RetargetAction::StopAndReplan);
}

#[test]
fn test_val_write_during_stop_deceleration_reissues_move() {
    // C parity: the kohzuCtl retarget sequence is a STOP put followed by a
    // VAL rewrite a few ms later, while the axis is still decelerating.
    // C do_work (motorRecord.cc:2241) dispatches the new move immediately
    // (the move block never tests MIP_STOP); the stop must not eat it.
    let mut rec = MotorRecord::new();
    rec.conv.mres = 0.01;
    rec.limits.dhlm = 100.0;
    rec.limits.dllm = -100.0;
    rec.stat.msta = MstaFlags::DONE;

    // Start a move to 50
    rec.pos.dval = 50.0;
    rec.pos.val = 50.0;
    let _ = rec.plan_motion(CommandSource::Val);
    assert_eq!(rec.stat.phase, MotionPhase::MainMove);

    // Axis under way at 25 when a STOP put processes
    rec.pos.rbv = 25.0;
    rec.pos.drbv = 25.0;
    rec.pos.rrbv = 2500;
    rec.stat.msta = MstaFlags::MOVING;
    rec.ctrl.stop = true;
    let effects = rec.plan_motion(CommandSource::Stop);
    assert!(rec.stat.mip.contains(MipFlags::STOP));
    assert!(matches!(effects.commands[0], MotorCommand::Stop { .. }));
    // C parity: no eager sync at stop-put time; VAL keeps the old target
    // until the axis actually stops (postProcess sync).
    assert_eq!(rec.pos.val, 50.0);

    // New target written while still decelerating (driver not done yet)
    rec.pos.dval = 80.0;
    rec.pos.val = 80.0;
    let effects = rec.plan_motion(CommandSource::Val);
    assert!(
        matches!(effects.commands[0], MotorCommand::MoveAbsolute { .. }),
        "VAL during stop deceleration must re-issue the move"
    );
    assert_eq!(rec.stat.mip, MipFlags::MOVE); // stop state replaced

    // Driver finishes at the new target: completion finalizes there
    // instead of running the plain-stop VAL<-RBV sync.
    rec.pos.rbv = 80.0;
    rec.pos.drbv = 80.0;
    rec.pos.rrbv = 8000;
    rec.stat.msta = MstaFlags::DONE;
    let _ = rec.check_completion();
    assert!(rec.stat.dmov);
    assert_eq!(rec.pos.val, 80.0);
}

#[test]
fn test_val_write_during_stop_replan_supersedes_parked_target() {
    // An NTM StopAndReplan parks its retarget in pending_retarget while
    // the stop decelerates. A newer explicit VAL write during that window
    // must win: the parked target is dropped (invariant: pending_retarget
    // is Some only while MIP_STOP is committed) and the new move
    // dispatched, exactly as C's last do_work pass would.
    let mut rec = MotorRecord::new();
    rec.conv.mres = 0.01;
    rec.limits.dhlm = 100.0;
    rec.limits.dllm = -100.0;
    rec.timing.ntm = true;
    rec.timing.ntmf = 2.0;
    rec.stat.msta = MstaFlags::DONE;

    rec.pos.dval = 50.0;
    rec.pos.val = 50.0;
    let _ = rec.plan_motion(CommandSource::Val);

    // Opposite-direction retarget mid-move -> StopAndReplan parks -10
    rec.pos.rbv = 25.0;
    rec.pos.drbv = 25.0;
    rec.stat.msta = MstaFlags::MOVING;
    rec.pos.dval = -10.0;
    rec.pos.val = -10.0;
    let effects = rec.plan_motion(CommandSource::Val);
    assert!(matches!(effects.commands[0], MotorCommand::Stop { .. }));
    assert!(rec.stat.mip.contains(MipFlags::STOP));
    assert_eq!(rec.internal.pending_retarget, Some(-10.0));

    // Newer explicit target during the same stop window
    rec.pos.dval = 30.0;
    rec.pos.val = 30.0;
    let effects = rec.plan_motion(CommandSource::Val);
    assert!(matches!(
        effects.commands[0],
        MotorCommand::MoveAbsolute { .. }
    ));
    assert_eq!(rec.internal.pending_retarget, None);
    assert_eq!(rec.stat.mip, MipFlags::MOVE);

    // Completion at 30 finalizes there; the parked -10 must not resurrect.
    rec.pos.rbv = 30.0;
    rec.pos.drbv = 30.0;
    rec.pos.rrbv = 3000;
    rec.stat.msta = MstaFlags::DONE;
    let effects = rec.check_completion();
    assert!(rec.stat.dmov);
    assert_eq!(rec.pos.val, 30.0);
    assert!(effects.commands.is_empty());
}

#[test]
fn test_latent_stop_wins_over_same_pass_val_write() {
    // C do_work evaluates pmr->stop at the top of every put-processing
    // pass (motorRecord.cc ~1860): a bare .STOP write (no process)
    // followed by a VAL write that does process must execute the stop,
    // not the move, even though last_write was overtaken by VAL.
    let mut rec = MotorRecord::new();
    rec.conv.mres = 0.01;
    rec.limits.dhlm = 100.0;
    rec.limits.dllm = -100.0;
    rec.stat.msta = MstaFlags::DONE;

    rec.pos.dval = 50.0;
    rec.pos.val = 50.0;
    let _ = rec.plan_motion(CommandSource::Val);
    assert_eq!(rec.stat.phase, MotionPhase::MainMove);

    // Bare .STOP write latches ctrl.stop without processing; the next
    // process pass is triggered by a VAL write.
    rec.pos.rbv = 25.0;
    rec.pos.drbv = 25.0;
    rec.pos.rrbv = 2500;
    rec.stat.msta = MstaFlags::MOVING;
    rec.ctrl.stop = true;
    rec.pos.dval = 80.0;
    rec.pos.val = 80.0;
    let effects = rec.plan_motion(CommandSource::Val);
    assert!(matches!(effects.commands[0], MotorCommand::Stop { .. }));
    assert!(!effects.commands.iter().any(|c| matches!(
        c,
        MotorCommand::MoveAbsolute { .. } | MotorCommand::MoveRelative { .. }
    )));
    assert!(rec.stat.mip.contains(MipFlags::STOP));
    assert!(!rec.ctrl.stop); // consumed by the stop, not left latent

    // The overtaken target is discarded by the stop-completion sync,
    // exactly as C's (val != lval) && !(mip & MIP_STOP) gate does.
    rec.stat.msta = MstaFlags::DONE;
    let _ = rec.check_completion();
    assert!(rec.stat.dmov);
    assert_eq!(rec.pos.val, 25.0);
}

#[test]
fn test_ueip_eres_readback() {
    let mut rec = MotorRecord::new();
    rec.conv.mres = 0.001;
    rec.conv.ueip = true;
    rec.conv.eres = 0.002;
    let status = asyn_rs::interfaces::motor::MotorStatus {
        position: 10.0,
        encoder_position: 10.0,
        done: true,
        ..Default::default()
    };
    rec.process_motor_info(&status);
    // REP = round(10.0 / 0.002) = 5000
    // RRBV = REP = 5000 (UEIP=true)
    // DRBV = 5000 * 0.002 = 10.0
    assert_eq!(rec.pos.rep, 5000);
    assert_eq!(rec.pos.rrbv, 5000);
    assert_eq!(rec.pos.drbv, 10.0);
}

#[test]
fn test_ueip_eres_nan_fallback_to_mres() {
    let mut rec = MotorRecord::new();
    rec.conv.mres = 0.001;
    rec.conv.ueip = true;
    rec.conv.eres = f64::NAN;
    let status = asyn_rs::interfaces::motor::MotorStatus {
        position: 10.0,
        encoder_position: 10.0,
        done: true,
        ..Default::default()
    };
    rec.process_motor_info(&status);
    // Should fall back to MRES for both REP and DRBV
    assert_eq!(rec.pos.rep, 10000);
    assert_eq!(rec.pos.drbv, 10.0);
}

#[test]
fn test_ueip_false_uses_motor_position() {
    let mut rec = MotorRecord::new();
    rec.conv.mres = 0.001;
    rec.conv.ueip = false;
    rec.conv.eres = 0.002;
    let status = asyn_rs::interfaces::motor::MotorStatus {
        position: 10.0,
        encoder_position: 20.0,
        done: true,
        ..Default::default()
    };
    rec.process_motor_info(&status);
    // UEIP=false: uses RMP path with MRES
    assert_eq!(rec.pos.rmp, 10000);
    assert_eq!(rec.pos.rrbv, 10000); // RMP, not REP
    assert_eq!(rec.pos.drbv, 10.0); // rrbv * mres
}

#[test]
fn test_stup_triggers_status_refresh() {
    let mut rec = MotorRecord::new();
    rec.stat.stup = 1;
    let effects = rec.do_process();
    assert!(effects.status_refresh);
    assert_eq!(rec.stat.stup, 0);
    assert!(effects.commands.is_empty());
}

#[test]
fn test_stup_does_not_drop_concurrent_user_write() {
    // C handles STUP at the top of do_work and continues the pass. A user
    // write (last_write) arriving in the same cycle must still be planned —
    // the STUP must not early-return and discard it.
    let mut rec = MotorRecord::new();
    rec.put_field("HLM", EpicsValue::Double(1000.0)).unwrap();
    rec.put_field("LLM", EpicsValue::Double(-1000.0)).unwrap();
    rec.stat.stup = 1;
    // VAL write in the same process cycle as the STUP request.
    rec.put_field("VAL", EpicsValue::Double(100.0)).unwrap();
    rec.set_event(MotorEvent::UserWrite(CommandSource::Val));
    let effects = rec.do_process();
    // STUP honoured…
    assert!(effects.status_refresh);
    assert_eq!(rec.stat.stup, 0);
    // …and the move command is still planned, not dropped.
    assert!(
        effects
            .commands
            .iter()
            .any(|c| matches!(c, MotorCommand::MoveAbsolute { .. })),
        "concurrent VAL write must still produce a move"
    );
}

#[test]
fn test_hls_blocks_positive_move() {
    let mut rec = MotorRecord::new();
    rec.conv.mres = 0.01;
    rec.limits.dhlm = 100.0;
    rec.limits.dllm = -100.0;
    rec.limits.hls = true;
    rec.stat.msta = MstaFlags::DONE;
    rec.pos.dval = 50.0;

    let effects = rec.plan_motion(CommandSource::Val);
    assert!(effects.commands.is_empty());
    assert!(rec.stat.dmov); // no motion started
}

#[test]
fn test_hls_allows_negative_move() {
    let mut rec = MotorRecord::new();
    rec.conv.mres = 0.01;
    rec.limits.dhlm = 100.0;
    rec.limits.dllm = -100.0;
    rec.limits.hls = true;
    rec.stat.msta = MstaFlags::DONE;
    rec.pos.dval = -10.0; // negative direction

    let effects = rec.plan_motion(CommandSource::Val);
    assert!(!effects.commands.is_empty());
    assert!(!rec.stat.dmov);
}

#[test]
fn test_lls_blocks_negative_move() {
    let mut rec = MotorRecord::new();
    rec.conv.mres = 0.01;
    rec.limits.dhlm = 100.0;
    rec.limits.dllm = -100.0;
    rec.limits.lls = true;
    rec.stat.msta = MstaFlags::DONE;
    rec.pos.dval = -50.0;

    let effects = rec.plan_motion(CommandSource::Val);
    assert!(effects.commands.is_empty());
}

#[test]
fn test_lls_allows_positive_move() {
    let mut rec = MotorRecord::new();
    rec.conv.mres = 0.01;
    rec.limits.dhlm = 100.0;
    rec.limits.dllm = -100.0;
    rec.limits.lls = true;
    rec.stat.msta = MstaFlags::DONE;
    rec.pos.dval = 10.0;

    let effects = rec.plan_motion(CommandSource::Val);
    assert!(!effects.commands.is_empty());
}

#[test]
fn test_both_limits_block_all_moves() {
    let mut rec = MotorRecord::new();
    rec.conv.mres = 0.01;
    rec.limits.dhlm = 100.0;
    rec.limits.dllm = -100.0;
    rec.limits.hls = true;
    rec.limits.lls = true;
    rec.stat.msta = MstaFlags::DONE;

    rec.pos.dval = 10.0;
    let effects = rec.plan_motion(CommandSource::Val);
    assert!(effects.commands.is_empty());

    rec.pos.dval = -10.0;
    let effects = rec.plan_motion(CommandSource::Val);
    assert!(effects.commands.is_empty());
}

#[test]
fn test_hls_blocks_forward_jog() {
    let mut rec = MotorRecord::new();
    rec.limits.hls = true;
    rec.ctrl.jogf = true;
    let effects = rec.plan_motion(CommandSource::Jogf);
    assert!(effects.commands.is_empty());
    assert!(rec.stat.dmov);
}

#[test]
fn test_cnen_emits_set_closed_loop_with_gain_support() {
    let mut rec = MotorRecord::new();
    rec.stat.msta = MstaFlags::GAIN_SUPPORT; // controller supports gain
    rec.ctrl.cnen = true;
    let effects = rec.plan_motion(CommandSource::Cnen);
    assert_eq!(effects.commands.len(), 1);
    assert!(matches!(
        effects.commands[0],
        MotorCommand::SetClosedLoop { enable: true }
    ));
}

#[test]
fn test_cnen_false_emits_disable_with_gain_support() {
    let mut rec = MotorRecord::new();
    rec.stat.msta = MstaFlags::GAIN_SUPPORT;
    rec.ctrl.cnen = false;
    let effects = rec.plan_motion(CommandSource::Cnen);
    assert_eq!(effects.commands.len(), 1);
    assert!(matches!(
        effects.commands[0],
        MotorCommand::SetClosedLoop { enable: false }
    ));
}

#[test]
fn test_cnen_no_command_without_gain_support() {
    // C: case motorRecordCNEN drives the torque command only when MSTA
    // reports gain support. A driver without it gets nothing.
    let mut rec = MotorRecord::new();
    assert!(!rec.stat.msta.contains(MstaFlags::GAIN_SUPPORT));
    rec.ctrl.cnen = true;
    let effects = rec.plan_motion(CommandSource::Cnen);
    assert!(
        !effects
            .commands
            .iter()
            .any(|c| matches!(c, MotorCommand::SetClosedLoop { .. })),
        "no SetClosedLoop without GAIN_SUPPORT"
    );
}

#[test]
fn test_spmg_stop_defers_sync_and_dmov_to_completion() {
    // C top block (motorRecord.cc:1890-1906): SPMG=Stop while moving only
    // sets pp=TRUE and mip=MIP_STOP — the VAL<-RBV sync and DMOV happen
    // when the axis actually stops (postProcess), not at put time.
    let mut rec = MotorRecord::new();
    rec.stat.phase = MotionPhase::MainMove;
    rec.stat.mip = MipFlags::MOVE;
    rec.stat.dmov = false;
    rec.pos.val = 50.0;
    rec.pos.dval = 50.0;
    rec.pos.rbv = 25.0;
    rec.pos.drbv = 25.0;
    rec.pos.rrbv = 2500;

    rec.ctrl.spmg = SpmgMode::Stop;
    let effects = rec.plan_motion(CommandSource::Spmg);
    assert!(matches!(effects.commands[0], MotorCommand::Stop { .. }));
    // Not finalized at put time: still decelerating.
    assert!(!rec.stat.dmov);
    assert_eq!(rec.stat.mip, MipFlags::STOP);
    assert_eq!(rec.pos.val, 50.0, "no eager VAL<-RBV sync mid-deceleration");

    // Axis comes to rest at 30 — completion syncs and finalizes.
    rec.stat.msta = MstaFlags::DONE;
    rec.stat.movn = false;
    rec.pos.rbv = 30.0;
    rec.pos.drbv = 30.0;
    rec.pos.rrbv = 3000;
    let _ = rec.check_completion();
    assert!(rec.stat.dmov);
    assert_eq!(rec.stat.phase, MotionPhase::Idle);
    assert_eq!(rec.pos.val, 30.0, "synced to the actual rest position");
    assert_eq!(rec.pos.dval, 30.0);
}

#[test]
fn test_stop_during_retry_finalizes_immediately_without_sync() {
    // C 1874-1885: a stop while MIP_RETRY abandons the retry and reports
    // done at once (mip=MIP_DONE, dmov=TRUE) WITHOUT a readback sync —
    // the drive fields keep the user target.
    let mut rec = MotorRecord::new();
    rec.stat.phase = MotionPhase::Retry;
    rec.stat.mip = MipFlags::RETRY;
    rec.stat.dmov = false;
    rec.pos.val = 50.0;
    rec.pos.dval = 50.0;
    rec.pos.rbv = 49.9;
    rec.pos.drbv = 49.9;

    rec.put_field("STOP", EpicsValue::Short(1)).unwrap();
    let effects = rec.plan_motion(CommandSource::Stop);
    assert!(
        effects
            .commands
            .iter()
            .any(|c| matches!(c, MotorCommand::Stop { .. }))
    );
    assert!(rec.stat.dmov, "retry stop reports done immediately");
    assert_eq!(rec.stat.mip, MipFlags::empty());
    assert_eq!(rec.pos.val, 50.0, "target retained, no readback sync");
    assert_eq!(rec.pos.dval, 50.0);
}

#[test]
fn test_spmg_stop_preserves_queued_motion_behind_in_flight_stop() {
    // C: SPMG=Stop while a stop is already in flight takes the early
    // return (1874-1888) BEFORE the button/MIP clears — a home queued
    // behind that stop survives and postProcess re-fires it once the
    // axis stops.
    let mut rec = MotorRecord::new();
    rec.put_field("HLM", EpicsValue::Double(1000.0)).unwrap();
    rec.put_field("LLM", EpicsValue::Double(-1000.0)).unwrap();
    rec.pos.dval = 50.0;
    rec.plan_motion(CommandSource::Val);
    rec.stat.msta = MstaFlags::MOVING;
    rec.stat.movn = true;

    // HOMF while moving: stop first, queue the home.
    rec.ctrl.homf = true;
    rec.plan_motion(CommandSource::Homf);
    assert!(rec.stat.mip.contains(MipFlags::STOP));
    assert!(rec.internal.queued_motion.is_some());

    // SPMG=Stop while that stop is still decelerating.
    rec.ctrl.spmg = SpmgMode::Stop;
    let effects = rec.plan_motion(CommandSource::Spmg);
    assert!(matches!(effects.commands[0], MotorCommand::Stop { .. }));
    assert!(
        rec.internal.queued_motion.is_some(),
        "queued home survives the early return like C"
    );

    // Axis stops — the queued home fires.
    rec.stat.msta = MstaFlags::DONE;
    rec.stat.movn = false;
    let effects = rec.check_completion();
    assert!(
        effects
            .commands
            .iter()
            .any(|c| matches!(c, MotorCommand::Home { .. })),
        "queued home resumes at stop completion (C postProcess parity)"
    );
}

#[test]
fn test_spmg_pause_cancels_queued_home() {
    // C (1898-1906): Pause reaches `mip = MIP_STOP`, erasing
    // MIP_HOMF|MIP_STOP, and clear_buttons() drops the home buttons —
    // a Pause cancels a queued home and Go must NOT resume it.
    let mut rec = MotorRecord::new();
    rec.put_field("HLM", EpicsValue::Double(1000.0)).unwrap();
    rec.put_field("LLM", EpicsValue::Double(-1000.0)).unwrap();
    rec.pos.dval = 50.0;
    rec.plan_motion(CommandSource::Val);
    rec.stat.msta = MstaFlags::MOVING;
    rec.stat.movn = true;

    rec.ctrl.homf = true;
    rec.plan_motion(CommandSource::Homf);
    assert!(rec.internal.queued_motion.is_some());

    rec.ctrl.spmg = SpmgMode::Pause;
    rec.plan_motion(CommandSource::Spmg);
    assert!(
        rec.internal.queued_motion.is_none(),
        "Pause cancels the queued home"
    );
    assert!(!rec.ctrl.homf, "home button dropped (C clear_buttons)");

    // Stop completes; Go must not fire a home.
    rec.stat.msta = MstaFlags::DONE;
    rec.stat.movn = false;
    let _ = rec.check_completion();
    rec.internal.lspg = SpmgMode::Pause;
    rec.ctrl.spmg = SpmgMode::Go;
    let effects = rec.plan_motion(CommandSource::Spmg);
    assert!(
        !effects
            .commands
            .iter()
            .any(|c| matches!(c, MotorCommand::Home { .. })),
        "Go after Pause must not resume the canceled home"
    );
}

#[test]
fn test_spmg_pause_then_go_resumes_queued_jog_from_button() {
    // C: Pause's `mip = MIP_STOP` erases MIP_JOG_REQ (queued jog), but
    // the still-latched jog BUTTON survives; Go re-arms the jog from the
    // button (motorRecord.cc:1916-1918).
    let mut rec = MotorRecord::new();
    rec.put_field("HLM", EpicsValue::Double(1000.0)).unwrap();
    rec.put_field("LLM", EpicsValue::Double(-1000.0)).unwrap();
    rec.pos.dval = 50.0;
    rec.plan_motion(CommandSource::Val);
    rec.stat.msta = MstaFlags::MOVING;
    rec.stat.movn = true;

    // JOGF while moving: stop first, queue the jog.
    rec.ctrl.jogf = true;
    rec.plan_motion(CommandSource::Jogf);
    assert!(rec.internal.queued_motion.is_some());

    rec.ctrl.spmg = SpmgMode::Pause;
    rec.plan_motion(CommandSource::Spmg);
    assert!(
        rec.internal.queued_motion.is_none(),
        "Pause cancels the queued jog (C: MIP_JOG_REQ erased)"
    );
    assert!(rec.ctrl.jogf, "jog button stays latched through Pause");

    // Stop completes while paused — no jog resume yet.
    rec.stat.msta = MstaFlags::DONE;
    rec.stat.movn = false;
    let effects = rec.check_completion();
    assert!(
        !effects
            .commands
            .iter()
            .any(|c| matches!(c, MotorCommand::MoveVelocity { .. })),
        "no jog resume while paused"
    );
    assert!(
        rec.ctrl.jogf,
        "jog button survives the pause-stop completion (C: no blanket \
         button clear at finalize)"
    );

    // Go resumes the jog from the latched button (C 1916-1918).
    rec.ctrl.spmg = SpmgMode::Go;
    let effects = rec.plan_motion(CommandSource::Spmg);
    assert!(
        effects.commands.iter().any(|c| matches!(
            c,
            MotorCommand::MoveVelocity {
                direction: true,
                ..
            }
        )),
        "Go re-arms the jog from the still-latched button"
    );
}

#[test]
fn test_queued_home_button_clears_at_home_completion() {
    // A home queued behind a stop keeps its button latched until the
    // resumed home completes (C postProcess re-fire keeps it, the
    // home-done path at 893-906 clears it).
    let mut rec = MotorRecord::new();
    rec.put_field("HLM", EpicsValue::Double(1000.0)).unwrap();
    rec.put_field("LLM", EpicsValue::Double(-1000.0)).unwrap();
    rec.pos.dval = 50.0;
    rec.plan_motion(CommandSource::Val);
    rec.stat.msta = MstaFlags::MOVING;
    rec.stat.movn = true;

    rec.ctrl.homf = true;
    rec.plan_motion(CommandSource::Homf);
    assert!(rec.internal.queued_motion.is_some());
    assert!(rec.ctrl.homf, "button stays latched while queued");

    // Stop completes → queued home fires.
    rec.stat.msta = MstaFlags::DONE;
    rec.stat.movn = false;
    let effects = rec.check_completion();
    assert!(
        effects
            .commands
            .iter()
            .any(|c| matches!(c, MotorCommand::Home { .. }))
    );
    assert_eq!(rec.stat.phase, MotionPhase::Homing);

    // Home completes → button cleared, no re-fire fuel left.
    rec.stat.msta = MstaFlags::DONE;
    rec.stat.movn = false;
    let _ = rec.check_completion();
    assert!(rec.stat.dmov);
    assert!(!rec.ctrl.homf, "home button cleared at home completion");
}

#[test]
fn test_latent_homf_fires_on_move_block_pass() {
    // A bare HOMF put overtaken in last_write must still fire: C's home
    // section evaluates the latched button state on every do_work pass
    // (motorRecord.cc:2010-2013) BEFORE the val move block, and returns
    // — the position write is overtaken.
    let mut rec = MotorRecord::new();
    rec.put_field("HLM", EpicsValue::Double(1000.0)).unwrap();
    rec.put_field("LLM", EpicsValue::Double(-1000.0)).unwrap();
    rec.ctrl.homf = true; // bare put, never dispatched

    rec.pos.val = 30.0;
    rec.pos.dval = 30.0;
    let effects = rec.plan_motion(CommandSource::Val);
    assert!(
        effects
            .commands
            .iter()
            .any(|c| matches!(c, MotorCommand::Home { forward: true, .. })),
        "latent HOMF must fire the home"
    );
    assert!(
        !effects
            .commands
            .iter()
            .any(|c| matches!(c, MotorCommand::MoveAbsolute { .. })),
        "home wins over the same-pass VAL write (C section order)"
    );
    assert_eq!(rec.stat.mip, MipFlags::HOMF);
    assert!(!rec.ctrl.homf, "button pulses off when the home starts");
    assert_eq!(rec.stat.phase, MotionPhase::Homing);
}

#[test]
fn test_latent_homf_during_move_stops_and_queues_home() {
    let mut rec = MotorRecord::new();
    rec.put_field("HLM", EpicsValue::Double(1000.0)).unwrap();
    rec.put_field("LLM", EpicsValue::Double(-1000.0)).unwrap();
    rec.pos.dval = 50.0;
    rec.plan_motion(CommandSource::Val);
    rec.stat.msta = MstaFlags::MOVING;
    rec.stat.movn = true;

    rec.ctrl.homf = true; // bare put while moving
    rec.pos.val = 80.0;
    rec.pos.dval = 80.0;
    let effects = rec.plan_motion(CommandSource::Val);
    assert!(
        effects
            .commands
            .iter()
            .any(|c| matches!(c, MotorCommand::Stop { .. })),
        "stop first, home after (C home-section movn branch)"
    );
    assert!(matches!(
        rec.internal.queued_motion,
        Some(QueuedMotion::Home { forward: true })
    ));
    // C 2023/2028: mip = MIP_HOMF wholesale, then the movn branch ORs in
    // MIP_STOP — the queued home is visible in the readback.
    assert_eq!(rec.stat.mip, MipFlags::HOMF | MipFlags::STOP);
}

#[test]
fn test_latent_jogf_fires_on_move_block_pass() {
    let mut rec = MotorRecord::new();
    rec.put_field("HLM", EpicsValue::Double(1000.0)).unwrap();
    rec.put_field("LLM", EpicsValue::Double(-1000.0)).unwrap();
    rec.ctrl.jogf = true; // bare put, never dispatched

    rec.pos.val = 30.0;
    rec.pos.dval = 30.0;
    let effects = rec.plan_motion(CommandSource::Val);
    assert!(
        effects.commands.iter().any(|c| matches!(
            c,
            MotorCommand::MoveVelocity {
                direction: true,
                ..
            }
        )),
        "latent JOGF must start the jog (C jog section, 2079)"
    );
    assert!(
        !effects
            .commands
            .iter()
            .any(|c| matches!(c, MotorCommand::MoveAbsolute { .. })),
        "jog wins over the same-pass VAL write"
    );
    assert_eq!(rec.stat.mip, MipFlags::JOGF);
}

#[test]
fn test_latent_jog_release_stops_jog() {
    // A bare JOGF=0 put (button released) overtaken in last_write must
    // still stop the jog: C's jog-stop section (2148) fires on state.
    let mut rec = MotorRecord::new();
    rec.put_field("HLM", EpicsValue::Double(1000.0)).unwrap();
    rec.put_field("LLM", EpicsValue::Double(-1000.0)).unwrap();
    rec.ctrl.jogf = true;
    rec.plan_motion(CommandSource::Jogf);
    assert_eq!(rec.stat.phase, MotionPhase::Jog);

    rec.ctrl.jogf = false; // bare release
    rec.pos.val = 30.0;
    rec.pos.dval = 30.0;
    let effects = rec.plan_motion(CommandSource::Val);
    assert!(
        effects
            .commands
            .iter()
            .any(|c| matches!(c, MotorCommand::Stop { .. })),
        "latent release must stop the jog"
    );
    assert!(rec.stat.mip.contains(MipFlags::JOG_STOP));
    assert_eq!(rec.stat.phase, MotionPhase::JogStopping);
}

#[test]
fn test_latent_opposite_jog_stops_and_queues_reverse() {
    let mut rec = MotorRecord::new();
    rec.put_field("HLM", EpicsValue::Double(1000.0)).unwrap();
    rec.put_field("LLM", EpicsValue::Double(-1000.0)).unwrap();
    rec.ctrl.jogf = true;
    rec.plan_motion(CommandSource::Jogf);
    rec.stat.msta = MstaFlags::MOVING;
    rec.stat.movn = true;

    // Bare puts: release JOGF, press JOGR.
    rec.ctrl.jogf = false;
    rec.ctrl.jogr = true;
    rec.pos.val = 30.0;
    rec.pos.dval = 30.0;
    let effects = rec.plan_motion(CommandSource::Val);
    assert!(
        effects
            .commands
            .iter()
            .any(|c| matches!(c, MotorCommand::Stop { .. })),
        "reversal stops the forward jog first"
    );
    assert!(matches!(
        rec.internal.queued_motion,
        Some(QueuedMotion::Jog { forward: false })
    ));
}

#[test]
fn test_latent_home_blocked_by_limit_falls_through_to_val() {
    // C's home gate includes the direction's limit switch (2010-2013):
    // with HLS active the HOMF clause fails and the pass falls through
    // to the move block — the VAL write must proceed.
    let mut rec = MotorRecord::new();
    rec.put_field("HLM", EpicsValue::Double(1000.0)).unwrap();
    rec.put_field("LLM", EpicsValue::Double(-1000.0)).unwrap();
    rec.ctrl.homf = true;
    rec.limits.hls = true;

    rec.pos.val = -30.0;
    rec.pos.dval = -30.0;
    let effects = rec.plan_motion(CommandSource::Val);
    assert!(
        !effects
            .commands
            .iter()
            .any(|c| matches!(c, MotorCommand::Home { .. })),
        "blocked home direction must not fire"
    );
    assert!(
        effects
            .commands
            .iter()
            .any(|c| matches!(c, MotorCommand::MoveAbsolute { .. })),
        "the VAL write proceeds like C's gate failing into the move block"
    );
}

#[test]
fn test_held_jog_button_does_not_refire_during_own_jog() {
    let mut rec = MotorRecord::new();
    rec.put_field("HLM", EpicsValue::Double(1000.0)).unwrap();
    rec.put_field("LLM", EpicsValue::Double(-1000.0)).unwrap();
    rec.ctrl.jogf = true;
    rec.plan_motion(CommandSource::Jogf);
    assert_eq!(rec.stat.phase, MotionPhase::Jog);

    // VAL write while the jog button is still held: no second jog
    // command, no jog stop, and the position write is ignored while
    // jogging (pre-existing retarget semantics).
    rec.pos.val = 30.0;
    rec.pos.dval = 30.0;
    let effects = rec.plan_motion(CommandSource::Val);
    assert!(effects.commands.is_empty());
    assert_eq!(rec.stat.phase, MotionPhase::Jog);
    assert!(rec.stat.mip.contains(MipFlags::JOGF));
}

#[test]
fn test_latent_home_wins_over_latent_jog() {
    // C section order: the home section (2010) runs before the jog
    // section (2079) — with both buttons latched, home fires.
    let mut rec = MotorRecord::new();
    rec.put_field("HLM", EpicsValue::Double(1000.0)).unwrap();
    rec.put_field("LLM", EpicsValue::Double(-1000.0)).unwrap();
    rec.ctrl.homf = true;
    rec.ctrl.jogf = true;

    rec.pos.val = 30.0;
    rec.pos.dval = 30.0;
    let effects = rec.plan_motion(CommandSource::Val);
    assert!(
        effects
            .commands
            .iter()
            .any(|c| matches!(c, MotorCommand::Home { .. })),
        "home outranks jog (C do_work order)"
    );
    assert!(
        !effects
            .commands
            .iter()
            .any(|c| matches!(c, MotorCommand::MoveVelocity { .. }))
    );
}

#[test]
fn test_latent_spmg_stop_halts_in_flight_motion() {
    // A bare SPMG=Stop put (dbPut, no process) overtaken in last_write
    // by a VAL write must still halt the axis on that pass — C compares
    // SPMG to LSPG by state at the top of every do_work pass
    // (motorRecord.cc:1855-1932) and returns, discarding the position
    // write.
    let mut rec = MotorRecord::new();
    rec.put_field("HLM", EpicsValue::Double(1000.0)).unwrap();
    rec.put_field("LLM", EpicsValue::Double(-1000.0)).unwrap();
    rec.pos.dval = 50.0;
    rec.plan_motion(CommandSource::Val);
    rec.stat.msta = MstaFlags::MOVING;
    rec.stat.movn = true;

    // Bare SPMG=Stop, then a processed VAL write wins last_write.
    rec.ctrl.spmg = SpmgMode::Stop;
    rec.pos.val = 80.0;
    rec.pos.dval = 80.0;
    let effects = rec.plan_motion(CommandSource::Val);
    assert!(
        effects
            .commands
            .iter()
            .any(|c| matches!(c, MotorCommand::Stop { .. })),
        "latent SPMG=Stop must halt the in-flight motion"
    );
    assert!(
        !effects.commands.iter().any(|c| matches!(
            c,
            MotorCommand::MoveAbsolute { .. } | MotorCommand::MoveRelative { .. }
        )),
        "the overtaken VAL write must not start a move"
    );
    assert_eq!(rec.stat.mip, MipFlags::STOP);
    assert_eq!(rec.internal.lspg, SpmgMode::Stop, "LSPG synced like C 1858");
}

#[test]
fn test_latent_spmg_pause_halts_in_flight_motion() {
    let mut rec = MotorRecord::new();
    rec.put_field("HLM", EpicsValue::Double(1000.0)).unwrap();
    rec.put_field("LLM", EpicsValue::Double(-1000.0)).unwrap();
    rec.pos.dval = 50.0;
    rec.plan_motion(CommandSource::Val);
    rec.stat.msta = MstaFlags::MOVING;
    rec.stat.movn = true;

    rec.ctrl.spmg = SpmgMode::Pause;
    rec.pos.val = 80.0;
    rec.pos.dval = 80.0;
    let effects = rec.plan_motion(CommandSource::Val);
    assert!(
        effects
            .commands
            .iter()
            .any(|c| matches!(c, MotorCommand::Stop { .. })),
        "latent SPMG=Pause must halt the in-flight motion"
    );
    assert!(!effects.commands.iter().any(|c| matches!(
        c,
        MotorCommand::MoveAbsolute { .. } | MotorCommand::MoveRelative { .. }
    )));
    assert!(rec.stat.mip.contains(MipFlags::STOP));
    // Pause preserves the drive fields for a Go resume.
    assert_eq!(rec.pos.dval, 80.0);
}

#[test]
fn test_latent_spmg_go_resumes_without_double_plan() {
    // Paused with an unreached target; a bare SPMG=Go put overtaken by a
    // VAL write. The latent Go's resume already targets the freshest
    // DVAL, so the pass must emit exactly one move.
    let mut rec = MotorRecord::new();
    rec.put_field("HLM", EpicsValue::Double(1000.0)).unwrap();
    rec.put_field("LLM", EpicsValue::Double(-1000.0)).unwrap();
    rec.conv.mres = 0.01;
    rec.internal.lspg = SpmgMode::Pause;
    rec.ctrl.spmg = SpmgMode::Pause;
    rec.pos.drbv = 25.0;
    rec.pos.rbv = 25.0;

    // Bare Go + processed VAL=80 in the same pass.
    rec.ctrl.spmg = SpmgMode::Go;
    rec.pos.val = 80.0;
    rec.pos.dval = 80.0;
    let effects = rec.plan_motion(CommandSource::Val);
    let moves: Vec<_> = effects
        .commands
        .iter()
        .filter(|c| {
            matches!(
                c,
                MotorCommand::MoveAbsolute { .. } | MotorCommand::MoveRelative { .. }
            )
        })
        .collect();
    assert_eq!(moves.len(), 1, "exactly one move — no double plan");
    assert!(
        matches!(moves[0], MotorCommand::MoveAbsolute { position, .. } if (position - 80.0).abs() < 1e-9),
        "the resume targets the freshest DVAL, got {:?}",
        moves[0]
    );
    assert_eq!(rec.internal.lspg, SpmgMode::Go);
}

#[test]
fn test_latent_spmg_go_without_resume_falls_through_to_src() {
    // A latent Stop→Go transition has nothing to resume (C only re-arms
    // from Pause); the pass falls through to normal dispatch like C's
    // Go branch falling through to the rest of do_work.
    let mut rec = MotorRecord::new();
    rec.put_field("HLM", EpicsValue::Double(1000.0)).unwrap();
    rec.put_field("LLM", EpicsValue::Double(-1000.0)).unwrap();
    rec.conv.mres = 0.01;
    rec.internal.lspg = SpmgMode::Stop;
    rec.ctrl.spmg = SpmgMode::Go;
    rec.pos.val = 30.0;
    rec.pos.dval = 30.0;
    let effects = rec.plan_motion(CommandSource::Val);
    assert!(
        effects
            .commands
            .iter()
            .any(|c| matches!(c, MotorCommand::MoveAbsolute { .. })),
        "the VAL write dispatches normally after the consumed Go"
    );
    assert_eq!(rec.internal.lspg, SpmgMode::Go);
}

#[test]
fn test_latent_stop_consumes_latent_spmg_go() {
    // A latent stop pulse and a latent Pause→Go in the same pass: C
    // consumes both at once (1858: lspg <- spmg before the stop branch),
    // so the Go must not be replayed as a resume on a later pass.
    let mut rec = MotorRecord::new();
    rec.put_field("HLM", EpicsValue::Double(1000.0)).unwrap();
    rec.put_field("LLM", EpicsValue::Double(-1000.0)).unwrap();
    rec.conv.mres = 0.01;
    rec.internal.lspg = SpmgMode::Pause;
    rec.ctrl.spmg = SpmgMode::Pause;
    rec.pos.drbv = 25.0;
    rec.pos.rbv = 25.0;
    rec.pos.val = 80.0;
    rec.pos.dval = 80.0;

    // Both bare: stop pulse + Go, then a TWF write processes the record.
    rec.ctrl.stop = true;
    rec.ctrl.spmg = SpmgMode::Go;
    rec.ctrl.twf = true;
    let effects = rec.plan_motion(CommandSource::Twf);
    assert!(
        effects
            .commands
            .iter()
            .any(|c| matches!(c, MotorCommand::Stop { .. })),
        "the latent stop wins the pass"
    );
    assert_eq!(rec.internal.lspg, SpmgMode::Go, "Go consumed with the stop");

    // A later pass dispatches normally: a replayed Go-resume would win
    // the pass (swallowing the tweak) and target the stale 80.
    rec.ctrl.twv = 5.0;
    rec.ctrl.twf = true;
    let effects = rec.plan_motion(CommandSource::Twf);
    assert!(
        !rec.ctrl.twf,
        "tweak dispatched, not swallowed by a replayed Go"
    );
    assert!(
        effects.commands.iter().any(|c| matches!(
            c,
            MotorCommand::MoveAbsolute { position, .. } if (position - 85.0).abs() < 1e-9
        )),
        "the tweak targets VAL+TWV=85; a replayed Go would have targeted 80"
    );
}

#[test]
fn test_stop_field_clears_latched_buttons_while_moving() {
    // C 1891-1893: the stop-while-moving branch calls clear_buttons() —
    // a latched jog/home button must not survive an explicit stop and
    // re-fire later.
    let mut rec = MotorRecord::new();
    rec.put_field("HLM", EpicsValue::Double(1000.0)).unwrap();
    rec.put_field("LLM", EpicsValue::Double(-1000.0)).unwrap();
    rec.pos.dval = 50.0;
    rec.plan_motion(CommandSource::Val);
    rec.stat.msta = MstaFlags::MOVING;
    rec.stat.movn = true;

    // Buttons latched by bare puts that never processed the record.
    rec.ctrl.jogf = true;
    rec.ctrl.homr = true;

    rec.put_field("STOP", EpicsValue::Short(1)).unwrap();
    rec.plan_motion(CommandSource::Stop);
    assert!(!rec.ctrl.jogf, "stop clears the latched jog button");
    assert!(!rec.ctrl.homr, "stop clears the latched home button");
    assert_eq!(rec.stat.mip, MipFlags::STOP);
}

// --- ACCS / ACCU (C: 36177f7b, 7b87f3b9, 63bfe5d0, 7291b556) ---

#[test]
fn test_accs_default_mirrors_accl() {
    let rec = MotorRecord::new();
    assert_eq!(rec.vel.accu, AccsUsed::Accl);
    // (velo=1.0 - vbas=0.0) / accl=0.2 == 5.0
    assert!((rec.vel.accs - 5.0).abs() < 1e-12);
}

#[test]
fn test_put_accl_recalculates_accs_and_switches_accu() {
    let mut rec = MotorRecord::new();
    rec.vel.velo = 10.0;
    rec.vel.vbas = 0.0;
    rec.vel.accu = AccsUsed::Accs; // start from Accs
    rec.put_field("ACCL", EpicsValue::Double(2.0)).unwrap();
    assert_eq!(rec.vel.accu, AccsUsed::Accl);
    assert!((rec.vel.accl - 2.0).abs() < 1e-12);
    // ACCS = (10 - 0) / 2 = 5
    assert!((rec.vel.accs - 5.0).abs() < 1e-12);
}

#[test]
fn test_put_accs_recalculates_accl_and_switches_accu() {
    let mut rec = MotorRecord::new();
    rec.vel.velo = 10.0;
    rec.vel.vbas = 0.0;
    rec.put_field("ACCS", EpicsValue::Double(4.0)).unwrap();
    assert_eq!(rec.vel.accu, AccsUsed::Accs);
    assert!((rec.vel.accs - 4.0).abs() < 1e-12);
    // ACCL = (10 - 0) / 4 = 2.5
    assert!((rec.vel.accl - 2.5).abs() < 1e-12);
}

#[test]
fn test_put_velo_cascades_to_accs_when_accu_is_accl() {
    let mut rec = MotorRecord::new();
    rec.vel.accu = AccsUsed::Accl;
    rec.vel.accl = 2.0;
    rec.vel.vbas = 0.0;
    rec.put_field("VELO", EpicsValue::Double(20.0)).unwrap();
    // ACCS = (20 - 0) / 2 = 10
    assert!((rec.vel.accs - 10.0).abs() < 1e-12);
    // ACCL unchanged
    assert!((rec.vel.accl - 2.0).abs() < 1e-12);
}

#[test]
fn test_put_velo_cascades_to_accl_when_accu_is_accs() {
    let mut rec = MotorRecord::new();
    rec.vel.accu = AccsUsed::Accs;
    rec.vel.accs = 5.0;
    rec.vel.vbas = 0.0;
    rec.put_field("VELO", EpicsValue::Double(20.0)).unwrap();
    // ACCL = (20 - 0) / 5 = 4
    assert!((rec.vel.accl - 4.0).abs() < 1e-12);
    assert!((rec.vel.accs - 5.0).abs() < 1e-12);
}

#[test]
fn test_put_vbas_cascades_per_accu() {
    let mut rec = MotorRecord::new();
    rec.vel.velo = 10.0;
    rec.vel.accu = AccsUsed::Accl;
    rec.vel.accl = 2.0;
    rec.put_field("VBAS", EpicsValue::Double(2.0)).unwrap();
    // ACCS = (10 - 2) / 2 = 4
    assert!((rec.vel.accs - 4.0).abs() < 1e-12);
}

#[test]
fn test_put_accu_does_not_recompute() {
    let mut rec = MotorRecord::new();
    rec.vel.accl = 2.0;
    rec.vel.accs = 5.0;
    rec.put_field("ACCU", EpicsValue::Short(1)).unwrap();
    assert_eq!(rec.vel.accu, AccsUsed::Accs);
    // No recompute on bare ACCU put — autosave path
    assert!((rec.vel.accl - 2.0).abs() < 1e-12);
    assert!((rec.vel.accs - 5.0).abs() < 1e-12);
}

// --- STOP is sent unconditionally (C: motorRecord.cc "just in case") ---

#[test]
fn test_stop_emits_command_even_when_record_idle() {
    // After an InPosition retry the record finalizes to Idle while the servo
    // may still be settling; a STOP must still reach the driver.
    let mut rec = MotorRecord::new();
    assert_eq!(rec.stat.phase, MotionPhase::Idle);
    rec.put_field("STOP", EpicsValue::Short(1)).unwrap();
    let effects = rec.plan_motion(CommandSource::Stop);
    assert!(
        effects
            .commands
            .iter()
            .any(|c| matches!(c, MotorCommand::Stop { .. })),
        "STOP must be forwarded even when the record is idle"
    );
}

#[test]
fn test_spmg_stop_emits_command_even_when_record_idle() {
    let mut rec = MotorRecord::new();
    rec.ctrl.spmg = SpmgMode::Stop;
    let effects = rec.plan_motion(CommandSource::Spmg);
    assert!(
        effects
            .commands
            .iter()
            .any(|c| matches!(c, MotorCommand::Stop { .. }))
    );
}

// --- DLY + STOP: DELAY wins (C: 38186d00 2017-03) ---

#[test]
fn test_stop_during_delaywait_emits_stop_but_keeps_waiting() {
    let mut rec = MotorRecord::new();
    rec.stat.phase = MotionPhase::DelayWait;
    rec.timing.dly = 1.0;
    rec.stat.dmov = false;
    rec.stat.mip.insert(MipFlags::DELAY_REQ);

    rec.put_field("STOP", EpicsValue::Short(1)).unwrap();
    let effects = rec.plan_motion(CommandSource::Stop);

    // STOP is forwarded to the controller
    assert!(
        effects
            .commands
            .iter()
            .any(|c| matches!(c, MotorCommand::Stop { .. }))
    );
    // DELAY wins: DMOV stays false until the timer expires
    assert!(!rec.stat.dmov);
}

#[test]
fn test_delaywait_finalizes_after_delay_expires() {
    // C 1441-1455: a poll landing during the DLY wait must NOT finalize —
    // only the watchdog expiry (DELAY_REQ -> DELAY_ACK) plus the status
    // refresh it requests drive the post-settle evaluation.
    let mut rec = MotorRecord::new();
    rec.stat.phase = MotionPhase::DelayWait;
    rec.stat.dmov = false;
    rec.stat.msta = MstaFlags::DONE;
    rec.stat.mip = MipFlags::MOVE | MipFlags::DELAY_REQ;
    // Poll tick mid-wait: stays waiting.
    rec.check_completion();
    assert_eq!(
        rec.stat.phase,
        MotionPhase::DelayWait,
        "poll must not truncate DLY"
    );
    assert!(!rec.stat.dmov);
    // Watchdog fires: REQ -> ACK, then the refreshed status lands and the
    // evaluation finalizes (diff == 0).
    rec.set_event(MotorEvent::DelayExpired);
    rec.do_process();
    assert!(rec.stat.mip.contains(MipFlags::DELAY_ACK));
    rec.check_completion();
    assert_eq!(rec.stat.phase, MotionPhase::Idle);
    assert!(rec.stat.dmov);
}

// --- RDBD validation / MRES==RDBD edge (C: cf984d50, 17168a52, 70063c0a) ---

#[test]
fn test_rdbd_forced_to_at_least_abs_mres() {
    // C: enforceMinRetryDeadband — RDBD must be >= |MRES|.
    let mut rec = MotorRecord::new();
    rec.conv.mres = 0.5;
    rec.put_field("RDBD", EpicsValue::Double(0.1)).unwrap();
    assert_eq!(rec.retry.rdbd, 0.5); // forced up to |MRES|

    rec.put_field("RDBD", EpicsValue::Double(2.0)).unwrap();
    assert_eq!(rec.retry.rdbd, 2.0); // larger value kept

    // Negative MRES: |MRES| used
    rec.conv.mres = -0.3;
    rec.put_field("RDBD", EpicsValue::Double(0.0)).unwrap();
    assert_eq!(rec.retry.rdbd, 0.3);
}

#[test]
fn test_mres_change_enforces_min_retry_deadband() {
    // C: special() calls enforceMinRetryDeadband on MRES change.
    let mut rec = MotorRecord::new();
    rec.conv.mres = 0.001;
    rec.retry.rdbd = 0.0005; // below |mres| after the change below
    rec.put_field("MRES", EpicsValue::Double(0.002)).unwrap();
    assert!(rec.retry.rdbd >= 0.002, "RDBD must be raised to |MRES|");
}

#[test]
fn test_do_process_enforces_min_retry_deadband() {
    // C: do_work calls enforceMinRetryDeadband every pass. A default RDBD of
    // 0.0 would disable retry entirely; do_process must floor it to |MRES|.
    let mut rec = MotorRecord::new();
    rec.conv.mres = 1.0;
    rec.retry.rdbd = 0.0;
    rec.do_process();
    assert_eq!(rec.retry.rdbd, 1.0);
}

#[test]
fn test_no_retry_when_diff_equals_rdbd() {
    // C: 70063c0a — retry only when diff strictly exceeds RDBD.
    let mut rec = MotorRecord::new();
    rec.stat.msta = MstaFlags::DONE;
    rec.stat.phase = MotionPhase::MainMove;
    rec.retry.rdbd = 0.5;
    rec.retry.rtry = 3;
    rec.pos.dval = 10.0;
    rec.pos.drbv = 9.5; // diff == 0.5 == rdbd → within deadband

    rec.check_completion();
    assert_eq!(rec.stat.phase, MotionPhase::Idle);
    assert_eq!(rec.retry.rcnt, 0); // no retry
}

#[test]
fn test_retry_when_diff_exceeds_rdbd() {
    let mut rec = MotorRecord::new();
    rec.stat.msta = MstaFlags::DONE;
    rec.stat.phase = MotionPhase::MainMove;
    rec.retry.rdbd = 0.5;
    rec.retry.rtry = 3;
    rec.pos.dval = 10.0;
    rec.pos.drbv = 9.0; // diff == 1.0 > rdbd

    rec.check_completion();
    assert_eq!(rec.stat.phase, MotionPhase::Retry);
    assert_eq!(rec.retry.rcnt, 1);
}

// --- 64-bit raw fields (epics-modules/motor #192) ---

#[test]
fn test_rval_holds_value_beyond_32bit_range() {
    // 3e9 exceeds i32::MAX (~2.15e9); must round-trip without overflow.
    let big: i64 = 3_000_000_000;
    let mut rec = MotorRecord::new();
    rec.conv.mres = 1.0;
    rec.conv.set = true; // SET mode: RVAL write redefines, no move
    rec.put_field("RVAL", EpicsValue::Int64(big)).unwrap();
    assert_eq!(rec.pos.rval, big);
    assert_eq!(rec.get_field("RVAL"), Some(EpicsValue::Int64(big)));
}

#[test]
fn test_rrbv_rmp_rep_are_int64_fields() {
    use asyn_rs::interfaces::motor::MotorStatus;
    let mut rec = MotorRecord::new();
    rec.conv.mres = 1.0;
    // Driver position beyond 32-bit
    let status = MotorStatus {
        position: 5_000_000_000.0,
        encoder_position: 5_000_000_000.0,
        ..Default::default()
    };
    rec.process_motor_info(&status);
    assert_eq!(rec.pos.rmp, 5_000_000_000_i64);
    match rec.get_field("RRBV") {
        Some(EpicsValue::Int64(v)) => assert_eq!(v, 5_000_000_000),
        other => panic!("RRBV should be Int64, got {other:?}"),
    }
    match rec.get_field("RMP") {
        Some(EpicsValue::Int64(_)) => {}
        other => panic!("RMP should be Int64, got {other:?}"),
    }
    match rec.get_field("REP") {
        Some(EpicsValue::Int64(_)) => {}
        other => panic!("REP should be Int64, got {other:?}"),
    }
}

#[test]
fn test_rval_put_accepts_legacy_long() {
    // Older 32-bit clients may still send Long; it must be accepted.
    let mut rec = MotorRecord::new();
    rec.conv.mres = 1.0;
    rec.conv.set = true;
    rec.put_field("RVAL", EpicsValue::Long(12345)).unwrap();
    assert_eq!(rec.pos.rval, 12345);
}

// --- RA_PROBLEM does not trigger record-level stop (C: 95c0a4ca PR #109) ---

#[test]
fn test_ra_problem_does_not_emit_stop_command() {
    use asyn_rs::interfaces::motor::MotorStatus;
    let mut rec = MotorRecord::new();
    // Motion in progress
    rec.put_field("VAL", EpicsValue::Double(50.0)).unwrap();
    rec.plan_motion(CommandSource::Val);
    assert_eq!(rec.stat.phase, MotionPhase::MainMove);

    // Driver reports a problem mid-motion
    let status = MotorStatus {
        position: 10.0,
        problem: true,
        moving: true,
        done: false,
        ..Default::default()
    };
    rec.process_motor_info(&status);
    assert!(rec.stat.msta.contains(MstaFlags::PROBLEM));

    let effects = rec.check_completion();
    // Record must NOT issue its own Stop — that is the driver's responsibility.
    assert!(
        !effects
            .commands
            .iter()
            .any(|c| matches!(c, MotorCommand::Stop { .. })),
        "RA_PROBLEM must not trigger a record-issued STOP"
    );
}

#[test]
fn test_ra_problem_completes_normally_when_driver_done() {
    use asyn_rs::interfaces::motor::MotorStatus;
    let mut rec = MotorRecord::new();
    rec.put_field("VAL", EpicsValue::Double(10.0)).unwrap();
    rec.plan_motion(CommandSource::Val);

    // Driver finished but flags a problem — record still finalizes, no STOP.
    let status = MotorStatus {
        position: 10.0,
        problem: true,
        moving: false,
        done: true,
        ..Default::default()
    };
    rec.process_motor_info(&status);
    let effects = rec.check_completion();
    assert!(
        !effects
            .commands
            .iter()
            .any(|c| matches!(c, MotorCommand::Stop { .. }))
    );
}

// --- Acceleration wire model: EGU/sec² (C: accEGUfromVelo) ---

#[test]
fn test_move_accel_is_egu_per_sec_squared() {
    // VELO=10, VBAS=0, ACCL=2 → accel = (10-0)/2 = 5 EGU/s²
    let mut rec = MotorRecord::new();
    rec.vel.vbas = 0.0;
    rec.put_field("VELO", EpicsValue::Double(10.0)).unwrap();
    rec.put_field("ACCL", EpicsValue::Double(2.0)).unwrap();
    rec.put_field("VAL", EpicsValue::Double(50.0)).unwrap();
    let effects = rec.plan_motion(CommandSource::Val);
    let accel = effects.commands.iter().find_map(|c| match c {
        MotorCommand::MoveAbsolute { acceleration, .. } => Some(*acceleration),
        MotorCommand::MoveRelative { acceleration, .. } => Some(*acceleration),
        _ => None,
    });
    assert_eq!(accel, Some(5.0));
}

#[test]
fn test_move_accel_uses_accs_directly_when_accu_is_accs() {
    let mut rec = MotorRecord::new();
    rec.vel.vbas = 0.0;
    rec.put_field("VELO", EpicsValue::Double(10.0)).unwrap();
    rec.put_field("ACCS", EpicsValue::Double(7.0)).unwrap(); // ACCU→Accs
    rec.put_field("VAL", EpicsValue::Double(50.0)).unwrap();
    let effects = rec.plan_motion(CommandSource::Val);
    let accel = effects.commands.iter().find_map(|c| match c {
        MotorCommand::MoveAbsolute { acceleration, .. } => Some(*acceleration),
        MotorCommand::MoveRelative { acceleration, .. } => Some(*acceleration),
        _ => None,
    });
    assert_eq!(accel, Some(7.0)); // ACCS used directly (already EGU/s²)
}

#[test]
fn test_move_accel_accu_accs_ignores_velo_vbas() {
    // C accEGUfromVelo: ACCU=Accs returns ACCS unconditionally — VELO/VBAS
    // do not enter the calculation.
    let mut rec = MotorRecord::new();
    rec.put_field("ACCS", EpicsValue::Double(4.0)).unwrap(); // ACCU→Accs
    rec.put_field("VELO", EpicsValue::Double(99.0)).unwrap();
    rec.put_field("VBAS", EpicsValue::Double(50.0)).unwrap();
    // re-assert ACCU=Accs (VELO/VBAS puts cascade but must not change accu)
    rec.put_field("ACCU", EpicsValue::Short(1)).unwrap();
    rec.put_field("VAL", EpicsValue::Double(500.0)).unwrap();
    let effects = rec.plan_motion(CommandSource::Val);
    let accel = effects.commands.iter().find_map(|c| match c {
        MotorCommand::MoveAbsolute { acceleration, .. } => Some(*acceleration),
        MotorCommand::MoveRelative { acceleration, .. } => Some(*acceleration),
        _ => None,
    });
    // ACCS is the master; the (VELO-VBAS)/ACCL path is not taken.
    assert_eq!(accel, Some(4.0));
}

#[test]
fn test_move_accel_velo_equals_vbas_fallback() {
    // VELO==VBAS: span==0 → fall back to VELO/ACCL (C: PR #75)
    let mut rec = MotorRecord::new();
    rec.put_field("VBAS", EpicsValue::Double(4.0)).unwrap();
    rec.put_field("VELO", EpicsValue::Double(4.0)).unwrap();
    rec.put_field("ACCL", EpicsValue::Double(2.0)).unwrap();
    rec.put_field("VAL", EpicsValue::Double(50.0)).unwrap();
    let effects = rec.plan_motion(CommandSource::Val);
    let accel = effects.commands.iter().find_map(|c| match c {
        MotorCommand::MoveAbsolute { acceleration, .. } => Some(*acceleration),
        MotorCommand::MoveRelative { acceleration, .. } => Some(*acceleration),
        _ => None,
    });
    // VELO/ACCL = 4/2 = 2, not 0
    assert_eq!(accel, Some(2.0));
}

fn emitted_accel(effects: &motor_rs::flags::ProcessEffects) -> Option<f64> {
    effects.commands.iter().find_map(|c| match c {
        MotorCommand::MoveAbsolute { acceleration, .. } => Some(*acceleration),
        MotorCommand::MoveRelative { acceleration, .. } => Some(*acceleration),
        _ => None,
    })
}

#[test]
fn test_move_accel_never_zero_for_unconfigured_axis() {
    // VELO==VBAS==0 (axis not yet configured) must not yield a 0 accel.
    let mut rec = MotorRecord::new();
    rec.vel.velo = 0.0;
    rec.vel.vbas = 0.0;
    rec.vel.accl = 0.5;
    rec.put_field("VAL", EpicsValue::Double(50.0)).unwrap();
    let effects = rec.plan_motion(CommandSource::Val);
    let a = emitted_accel(&effects).expect("a move command must be emitted for a 50-unit move");
    assert!(a > 0.0, "acceleration must stay positive, got {a}");
}

#[test]
fn test_accs_cascade_consistent_with_move_accel_under_vbas_unsupported() {
    use asyn_rs::interfaces::motor::MotorStatus;
    // VBAS_UNSUPPORTED axis: the ACCS field value and the driver-bound
    // acceleration must agree (both treat VBAS as 0).
    let mut rec = MotorRecord::new();
    rec.process_motor_info(&MotorStatus {
        vbas_supported: false,
        ..Default::default()
    });
    rec.vel.vbas = 6.0; // would skew accs if used raw
    rec.vel.accu = AccsUsed::Accl;
    rec.vel.accl = 2.0;
    rec.put_field("VELO", EpicsValue::Double(10.0)).unwrap();
    // apply_accu_cascade derived ACCS using effective VBAS (0): (10-0)/2 = 5
    assert!((rec.vel.accs - 5.0).abs() < 1e-12);
    // The driver-bound acceleration (ACCU=Accl path) must yield the same.
    rec.put_field("VAL", EpicsValue::Double(50.0)).unwrap();
    let effects = rec.plan_motion(CommandSource::Val);
    assert_eq!(emitted_accel(&effects), Some(5.0));
}

#[test]
fn test_move_accel_vbas_unsupported_treats_vbas_as_zero() {
    use asyn_rs::interfaces::motor::MotorStatus;
    // Driver advertises VBAS unsupported: accel math ignores VBAS.
    let mut rec = MotorRecord::new();
    rec.process_motor_info(&MotorStatus {
        vbas_supported: false,
        ..Default::default()
    });
    rec.put_field("VBAS", EpicsValue::Double(6.0)).unwrap(); // would skew accel
    rec.put_field("VELO", EpicsValue::Double(10.0)).unwrap();
    rec.put_field("ACCL", EpicsValue::Double(2.0)).unwrap();
    rec.put_field("VAL", EpicsValue::Double(50.0)).unwrap();
    let effects = rec.plan_motion(CommandSource::Val);
    let accel = effects.commands.iter().find_map(|c| match c {
        MotorCommand::MoveAbsolute { acceleration, .. } => Some(*acceleration),
        MotorCommand::MoveRelative { acceleration, .. } => Some(*acceleration),
        _ => None,
    });
    // VBAS treated as 0: (10-0)/2 = 5, not (10-6)/2 = 2
    assert_eq!(accel, Some(5.0));
}

// --- VBAS not supported MSTA bit (issue #76, unmerged PR #80/#81) ---

#[test]
fn test_vbas_unsupported_bit_set_when_driver_reports_false() {
    use asyn_rs::interfaces::motor::MotorStatus;
    let mut rec = MotorRecord::new();
    let status = MotorStatus {
        vbas_supported: false,
        ..Default::default()
    };
    rec.process_motor_info(&status);
    assert!(rec.stat.msta.contains(MstaFlags::VBAS_UNSUPPORTED));
}

#[test]
fn test_vbas_unsupported_bit_clear_by_default() {
    use asyn_rs::interfaces::motor::MotorStatus;
    let mut rec = MotorRecord::new();
    let status = MotorStatus::default(); // vbas_supported defaults to true
    rec.process_motor_info(&status);
    assert!(!rec.stat.msta.contains(MstaFlags::VBAS_UNSUPPORTED));
}

// --- moveToHome (C: a6f64591 + 5f421e9a 2011-07) ---

#[test]
fn test_move_to_home_defaults_to_move_absolute() {
    use asyn_rs::interfaces::motor::AsynMotor;
    use asyn_rs::user::AsynUser;
    let mut sim = motor_rs::sim_motor::SimMotor::new();
    let user = AsynUser::new(0);
    // move_to_home's default impl delegates to move_absolute(position=3.0).
    sim.move_to_home(&user, 3.0, 1.0, 0.5).unwrap();
    let status = sim.poll(&user).unwrap();
    // The sim must now be moving toward 3.0 from its start at 0.0.
    assert!(status.moving, "move_to_home should start a move");
}

// --- PCO record PV exposure (C: 05b25c1d PR #248) ---

#[test]
fn test_pco_config_fields_roundtrip() {
    let mut rec = MotorRecord::new();
    rec.put_field("PCO_START", EpicsValue::Double(-5.0))
        .unwrap();
    rec.put_field("PCO_END", EpicsValue::Double(5.0)).unwrap();
    rec.put_field("PCO_INC", EpicsValue::Double(0.25)).unwrap();
    rec.put_field("PCO_PW", EpicsValue::Double(3.0)).unwrap();
    assert_eq!(rec.get_field("PCO_START"), Some(EpicsValue::Double(-5.0)));
    assert_eq!(rec.get_field("PCO_END"), Some(EpicsValue::Double(5.0)));
    assert_eq!(rec.get_field("PCO_INC"), Some(EpicsValue::Double(0.25)));
    assert_eq!(rec.get_field("PCO_PW"), Some(EpicsValue::Double(3.0)));
}

#[test]
fn test_pco_enable_emits_config_then_enable() {
    let mut rec = MotorRecord::new();
    rec.put_field("PCO_START", EpicsValue::Double(-5.0))
        .unwrap();
    rec.put_field("PCO_END", EpicsValue::Double(5.0)).unwrap();
    rec.put_field("PCO_INC", EpicsValue::Double(0.25)).unwrap();
    rec.put_field("PCO_PW", EpicsValue::Double(3.0)).unwrap();
    rec.put_field("PCO_ENABLE", EpicsValue::Short(1)).unwrap();
    let effects = rec.plan_motion(CommandSource::PcoEnable);

    // SetPcoConfig must precede EnablePco
    assert!(matches!(
        effects.commands[0],
        MotorCommand::SetPcoConfig {
            start, end, increment, pulse_width_us
        } if start == -5.0 && end == 5.0 && increment == 0.25 && pulse_width_us == 3.0
    ));
    assert!(matches!(
        effects.commands[1],
        MotorCommand::EnablePco { enable: true }
    ));
}

#[test]
fn test_pco_enable_disable_roundtrip() {
    let mut rec = MotorRecord::new();
    rec.put_field("PCO_ENABLE", EpicsValue::Short(1)).unwrap();
    assert_eq!(rec.get_field("PCO_ENABLE"), Some(EpicsValue::Short(1)));
    let effects = rec.plan_motion(CommandSource::PcoEnable);
    assert!(matches!(
        effects.commands[1],
        MotorCommand::EnablePco { enable: true }
    ));

    rec.put_field("PCO_ENABLE", EpicsValue::Short(0)).unwrap();
    assert_eq!(rec.get_field("PCO_ENABLE"), Some(EpicsValue::Short(0)));
    let effects = rec.plan_motion(CommandSource::PcoEnable);
    assert!(matches!(
        effects.commands[1],
        MotorCommand::EnablePco { enable: false }
    ));
}

// --- enable_pco / set_pco_config (C: 05b25c1d PR #248) ---

#[test]
fn test_sim_motor_enable_pco_default_is_false() {
    let sim = motor_rs::sim_motor::SimMotor::new();
    assert!(!sim.pco_enabled);
}

#[test]
fn test_enable_pco_command_flows_through_axis() {
    use asyn_rs::interfaces::motor::AsynMotor;
    use asyn_rs::user::AsynUser;
    let mut sim = motor_rs::sim_motor::SimMotor::new();
    let user = AsynUser::new(0);
    sim.set_pco_config(&user, -10.0, 10.0, 0.5, 5.0).unwrap();
    sim.enable_pco(&user, true).unwrap();
    assert!(sim.pco_enabled);
    assert_eq!(sim.pco_start, -10.0);
    assert_eq!(sim.pco_end, 10.0);
    assert_eq!(sim.pco_increment, 0.5);
    assert_eq!(sim.pco_pulse_width_us, 5.0);
}

#[test]
fn test_default_asyn_motor_enable_pco_is_noop() {
    // Drivers without PCO inherit the trait default — must not error.
    use asyn_rs::interfaces::motor::AsynMotor;
    use asyn_rs::user::AsynUser;
    struct Plain;
    impl AsynMotor for Plain {
        fn move_absolute(
            &mut self,
            _: &AsynUser,
            _: f64,
            _: f64,
            _: f64,
        ) -> asyn_rs::error::AsynResult<()> {
            Ok(())
        }
        fn home(&mut self, _: &AsynUser, _: f64, _: bool) -> asyn_rs::error::AsynResult<()> {
            Ok(())
        }
        fn stop(&mut self, _: &AsynUser, _: f64) -> asyn_rs::error::AsynResult<()> {
            Ok(())
        }
        fn set_position(&mut self, _: &AsynUser, _: f64) -> asyn_rs::error::AsynResult<()> {
            Ok(())
        }
        fn poll(
            &mut self,
            _: &AsynUser,
        ) -> asyn_rs::error::AsynResult<asyn_rs::interfaces::motor::MotorStatus> {
            Ok(asyn_rs::interfaces::motor::MotorStatus::default())
        }
    }
    let mut plain = Plain;
    let user = AsynUser::new(0);
    plain.enable_pco(&user, true).unwrap();
    plain.set_pco_config(&user, 0.0, 0.0, 0.0, 0.0).unwrap();
}

// --- RVEL raw velocity (C: motorRecord.dbd DBF_LONG, devMotorAsyn) ---

#[test]
fn test_rvel_is_floored_raw_velocity() {
    use asyn_rs::interfaces::motor::MotorStatus;
    let mut rec = MotorRecord::new();
    let status = MotorStatus {
        velocity: 3.7,
        ..Default::default()
    };
    rec.process_motor_info(&status);
    // C devMotorAsyn: rvel = floor(status.velocity)
    assert_eq!(rec.get_field("RVEL"), Some(EpicsValue::Int64(3)));
}

#[test]
fn test_rvel_is_independent_of_velo_setpoint() {
    use asyn_rs::interfaces::motor::MotorStatus;
    let mut rec = MotorRecord::new();
    rec.put_field("VELO", EpicsValue::Double(10.0)).unwrap(); // setpoint
    let status = MotorStatus {
        velocity: 7.0,
        ..Default::default()
    };
    rec.process_motor_info(&status);
    // VELO setpoint and RVEL raw velocity are separate values
    assert_eq!(rec.get_field("VELO"), Some(EpicsValue::Double(10.0)));
    assert_eq!(rec.get_field("RVEL"), Some(EpicsValue::Int64(7)));
}

// --- ACCL safety when VELO==VBAS (C: b201e40e PR #75) ---

#[test]
fn test_accl_stays_positive_when_velo_equals_vbas() {
    // C: accEGUfromVelo(velo, vbas, accl) would emit 0 if velo==vbas.
    // motor-rs sends ACCL (sec) directly, so the floor enforced by the
    // ACCL put handler keeps the driver-bound value > 0.
    let mut rec = MotorRecord::new();
    rec.vel.vbas = 5.0;
    rec.put_field("VELO", EpicsValue::Double(5.0)).unwrap(); // VELO == VBAS
    // ACCL is whatever the user wrote (default 0.2); never derived to 0
    assert!(rec.vel.accl > 0.0);
    // Put ACCL=0 should be clamped to 0.1
    rec.put_field("ACCL", EpicsValue::Double(0.0)).unwrap();
    assert!(rec.vel.accl > 0.0);
    assert!((rec.vel.accl - 0.1).abs() < 1e-12);
}

#[test]
fn test_accu_cascade_safe_when_velo_equals_vbas() {
    // span <= 0.0 → apply_accu_cascade leaves master untouched.
    let mut rec = MotorRecord::new();
    rec.vel.accu = AccsUsed::Accl;
    rec.vel.accl = 2.0;
    rec.vel.accs = 5.0; // pre-existing slave value
    rec.vel.vbas = 5.0;
    rec.put_field("VELO", EpicsValue::Double(5.0)).unwrap();
    // ACCS slave should not be set to 0 just because span == 0
    assert!(rec.vel.accs > 0.0);
    assert!((rec.vel.accs - 5.0).abs() < 1e-12);
}

// --- Home ignores soft limit error (C: dbcf4bc2 2011-10, 85511023 2013-06) ---

#[test]
fn test_home_proceeds_when_position_outside_soft_limits() {
    // C: home must not be blocked by soft-limit error; the home seek may
    // move the axis back inside the window.
    let mut rec = MotorRecord::new();
    rec.put_field("HLM", EpicsValue::Double(10.0)).unwrap();
    rec.put_field("LLM", EpicsValue::Double(-10.0)).unwrap();
    // Position outside soft window
    rec.pos.val = 15.0;
    rec.pos.dval = 15.0;
    rec.ctrl.homf = true;
    let effects = rec.plan_motion(CommandSource::Homf);
    assert!(
        effects
            .commands
            .iter()
            .any(|c| matches!(c, MotorCommand::Home { .. })),
        "home command should still be emitted past soft limits"
    );
    assert_eq!(rec.stat.phase, MotionPhase::Homing);
}

#[test]
fn test_home_reverse_proceeds_below_low_soft_limit() {
    let mut rec = MotorRecord::new();
    rec.put_field("HLM", EpicsValue::Double(10.0)).unwrap();
    rec.put_field("LLM", EpicsValue::Double(-10.0)).unwrap();
    rec.pos.val = -15.0;
    rec.pos.dval = -15.0;
    rec.ctrl.homr = true;
    let effects = rec.plan_motion(CommandSource::Homr);
    assert!(
        effects
            .commands
            .iter()
            .any(|c| matches!(c, MotorCommand::Home { .. }))
    );
}

// --- SPMG=Go resumes a latched jog button (C motorSPMG_Go) ---

#[test]
fn test_spmg_go_resumes_latched_jog() {
    let mut rec = MotorRecord::new();
    rec.put_field("HLM", EpicsValue::Double(1000.0)).unwrap();
    rec.put_field("LLM", EpicsValue::Double(-1000.0)).unwrap();
    // Jog button latched while paused.
    rec.ctrl.jogf = true;
    rec.internal.lspg = SpmgMode::Pause;
    rec.ctrl.spmg = SpmgMode::Go;
    let effects = rec.plan_motion(CommandSource::Spmg);
    assert!(
        effects.commands.iter().any(|c| matches!(
            c,
            MotorCommand::MoveVelocity {
                direction: true,
                ..
            }
        )),
        "SPMG=Go must resume the latched forward jog"
    );
}

#[test]
fn test_spmg_go_without_jog_replans_to_dval() {
    let mut rec = MotorRecord::new();
    rec.conv.mres = 0.01;
    rec.put_field("HLM", EpicsValue::Double(1000.0)).unwrap();
    rec.put_field("LLM", EpicsValue::Double(-1000.0)).unwrap();
    rec.pos.dval = 50.0;
    rec.pos.drbv = 0.0;
    rec.internal.lspg = SpmgMode::Pause;
    rec.ctrl.spmg = SpmgMode::Go;
    let effects = rec.plan_motion(CommandSource::Spmg);
    // No jog latched → replan toward DVAL.
    assert!(effects.commands.iter().any(|c| matches!(
        c,
        MotorCommand::MoveAbsolute { .. } | MotorCommand::MoveRelative { .. }
    )));
}

// --- JOGF + JOGR simultaneous: latest-wins (epics-modules/motor #170) ---

#[test]
fn test_jogr_command_clears_latched_jogf() {
    let mut rec = MotorRecord::new();
    rec.put_field("HLM", EpicsValue::Double(1000.0)).unwrap();
    rec.put_field("LLM", EpicsValue::Double(-1000.0)).unwrap();
    // Both buttons latched (the ambiguous state #170 addresses)
    rec.ctrl.jogf = true;
    rec.ctrl.jogr = true;
    // Latest command is JOGR
    let effects = rec.plan_motion(CommandSource::Jogr);
    assert!(!rec.ctrl.jogf, "JOGR command must clear latched JOGF");
    // Reverse jog issued
    assert!(effects.commands.iter().any(|c| matches!(
        c,
        MotorCommand::MoveVelocity {
            direction: false,
            ..
        }
    )));
}

#[test]
fn test_jogf_command_clears_latched_jogr() {
    let mut rec = MotorRecord::new();
    rec.put_field("HLM", EpicsValue::Double(1000.0)).unwrap();
    rec.put_field("LLM", EpicsValue::Double(-1000.0)).unwrap();
    rec.ctrl.jogf = true;
    rec.ctrl.jogr = true;
    let effects = rec.plan_motion(CommandSource::Jogf);
    assert!(!rec.ctrl.jogr, "JOGF command must clear latched JOGR");
    assert!(effects.commands.iter().any(|c| matches!(
        c,
        MotorCommand::MoveVelocity {
            direction: true,
            ..
        }
    )));
}

// --- LVIO jog rejection (C: 9e5b5432 PR #99) ---

#[test]
fn test_forward_jog_at_high_limit_is_rejected_and_clears_button() {
    let mut rec = MotorRecord::new();
    rec.put_field("HLM", EpicsValue::Double(10.0)).unwrap();
    rec.put_field("LLM", EpicsValue::Double(-10.0)).unwrap();
    rec.pos.val = 10.0; // at high limit
    rec.pos.dval = 10.0;
    rec.ctrl.jogf = true;
    let effects = rec.plan_motion(CommandSource::Jogf);
    assert!(
        !effects
            .commands
            .iter()
            .any(|c| matches!(c, MotorCommand::MoveVelocity { .. })),
        "forward jog at HLM should not emit MoveVelocity"
    );
    assert!(!rec.ctrl.jogf, "JOGF button cleared");
    assert!(rec.limits.lvio);
    assert_eq!(rec.stat.phase, MotionPhase::Idle);
}

#[test]
fn test_reverse_jog_at_low_limit_is_rejected_and_clears_button() {
    let mut rec = MotorRecord::new();
    rec.put_field("HLM", EpicsValue::Double(10.0)).unwrap();
    rec.put_field("LLM", EpicsValue::Double(-10.0)).unwrap();
    rec.pos.val = -10.0;
    rec.pos.dval = -10.0;
    rec.ctrl.jogr = true;
    rec.plan_motion(CommandSource::Jogr);
    assert!(!rec.ctrl.jogr);
    assert!(rec.limits.lvio);
}

#[test]
fn test_jog_soft_limit_uses_user_frame_with_dir_neg() {
    // C: d52fe5ee — jog soft-limit check is in user coordinates. With
    // DIR=Neg the user frame still has JOGF = user-positive, so a forward
    // jog at the user high limit is rejected regardless of DIR.
    let mut rec = MotorRecord::new();
    rec.conv.dir = MotorDir::Neg;
    rec.put_field("HLM", EpicsValue::Double(10.0)).unwrap();
    rec.put_field("LLM", EpicsValue::Double(-10.0)).unwrap();
    rec.pos.val = 10.0; // at user high limit
    rec.ctrl.jogf = true;
    rec.plan_motion(CommandSource::Jogf);
    assert!(
        !rec.ctrl.jogf,
        "forward jog at user HLM rejected under DIR=Neg"
    );
    assert!(rec.limits.lvio);
}

#[test]
fn test_reverse_jog_at_high_limit_is_allowed() {
    let mut rec = MotorRecord::new();
    rec.put_field("HLM", EpicsValue::Double(10.0)).unwrap();
    rec.put_field("LLM", EpicsValue::Double(-10.0)).unwrap();
    rec.pos.val = 10.0; // at high limit
    rec.pos.dval = 10.0;
    rec.ctrl.jogr = true;
    let effects = rec.plan_motion(CommandSource::Jogr);
    // Reverse jog back inside the window should proceed
    assert!(
        effects
            .commands
            .iter()
            .any(|c| matches!(c, MotorCommand::MoveVelocity { .. })),
        "reverse jog inside soft window should emit MoveVelocity"
    );
}

// --- Driver-initiated stop clears motion buttons (C: 9c8a8e8c PR #56) ---

#[test]
fn test_jog_finalize_clears_jog_button_when_driver_stops() {
    // phase=Jog, jogf still latched, driver reports done — record must
    // clear ctrl.jogf so the next process pass does not re-issue the jog.
    let mut rec = MotorRecord::new();
    rec.ctrl.jogf = true;
    rec.stat.mip.insert(MipFlags::JOGF);
    rec.stat.phase = MotionPhase::Jog;
    rec.stat.dmov = false;
    rec.retry.bdst = 0.0; // no jog backlash

    rec.stat.msta = MstaFlags::DONE;
    rec.check_completion();

    assert_eq!(rec.stat.phase, MotionPhase::Idle);
    assert!(!rec.ctrl.jogf, "JOGF button should clear on driver stop");
}

#[test]
fn test_stop_field_during_active_jog_does_not_resume_jog() {
    // STOP field pressed while a jog is actively running: the axis must
    // finalize to Idle, NOT replay the jog. An active jog leaves MIP=JOGF;
    // handle_stop adds STOP. The check_completion STOP branch must only
    // resume a *queued* request (internal.queued_motion), which is absent here.
    let mut rec = MotorRecord::new();
    rec.ctrl.jogf = true;
    rec.stat.mip.insert(MipFlags::JOGF);
    rec.stat.phase = MotionPhase::Jog;
    rec.stat.dmov = false;
    rec.retry.bdst = 0.0;

    // STOP field write → handle_stop adds MIP_STOP (mip = JOGF | STOP).
    rec.put_field("STOP", EpicsValue::Short(1)).unwrap();
    rec.plan_motion(CommandSource::Stop);
    assert!(rec.stat.mip.contains(MipFlags::STOP));

    // Driver reports done → check_completion must finalize, not re-jog.
    rec.stat.msta = MstaFlags::DONE;
    let effects = rec.check_completion();
    assert_eq!(
        rec.stat.phase,
        MotionPhase::Idle,
        "must finalize, not resume jog"
    );
    assert!(
        !effects
            .commands
            .iter()
            .any(|c| matches!(c, MotorCommand::MoveVelocity { .. })),
        "STOP on an active jog must not re-issue MoveVelocity"
    );
}

#[test]
fn test_jog_command_while_moving_is_queued_and_resumes_after_stop() {
    // A jog command issued while a move is in progress: the record stops the
    // current motion, then resumes the queued jog once the driver is done.
    let mut rec = MotorRecord::new();
    rec.put_field("HLM", EpicsValue::Double(1000.0)).unwrap();
    rec.put_field("LLM", EpicsValue::Double(-1000.0)).unwrap();
    // A move is in progress.
    rec.put_field("VAL", EpicsValue::Double(100.0)).unwrap();
    rec.plan_motion(CommandSource::Val);
    rec.stat.movn = true;

    // Jog command arrives mid-move → queued, current motion stopped.
    rec.ctrl.jogf = true;
    let effects = rec.plan_motion(CommandSource::Jogf);
    assert!(
        effects
            .commands
            .iter()
            .any(|c| matches!(c, MotorCommand::Stop { .. }))
    );

    // Driver reports done → queued jog resumes.
    rec.stat.msta = MstaFlags::DONE;
    let effects = rec.check_completion();
    assert_eq!(rec.stat.phase, MotionPhase::Jog);
    assert!(effects.commands.iter().any(|c| matches!(
        c,
        MotorCommand::MoveVelocity {
            direction: true,
            ..
        }
    )));
}

#[test]
fn test_homing_finalize_clears_home_button() {
    let mut rec = MotorRecord::new();
    rec.ctrl.homf = true;
    rec.stat.mip.insert(MipFlags::HOMF);
    rec.stat.phase = MotionPhase::Homing;
    rec.stat.dmov = false;

    rec.stat.msta = MstaFlags::DONE;
    rec.check_completion();

    assert_eq!(rec.stat.phase, MotionPhase::Idle);
    assert!(!rec.ctrl.homf, "HOMF button should clear on driver done");
}

// --- VAL/HOMF endless loop fix (C: 0aaf02d7 2025-02 PR #224) ---

#[test]
fn test_val_write_clears_homf_during_homing() {
    let mut rec = MotorRecord::new();
    rec.ctrl.homf = true;
    rec.stat.mip.insert(MipFlags::HOMF);
    rec.stat.phase = MotionPhase::Homing;

    rec.put_field("VAL", EpicsValue::Double(10.0)).unwrap();
    rec.plan_motion(CommandSource::Val);

    assert!(!rec.ctrl.homf);
    assert!(!rec.ctrl.homr);
}

#[test]
fn test_dval_write_clears_homr_during_reverse_homing() {
    let mut rec = MotorRecord::new();
    rec.ctrl.homr = true;
    rec.stat.mip.insert(MipFlags::HOMR);
    rec.stat.phase = MotionPhase::Homing;

    rec.put_field("DVAL", EpicsValue::Double(5.0)).unwrap();
    rec.plan_motion(CommandSource::Dval);

    assert!(!rec.ctrl.homr);
}

#[test]
fn test_val_write_does_not_clear_buttons_when_not_homing() {
    // If a regular move is in progress, VAL retarget shouldn't touch HOMF/HOMR.
    let mut rec = MotorRecord::new();
    rec.stat.mip.insert(MipFlags::MOVE);
    rec.stat.phase = MotionPhase::MainMove;
    rec.ctrl.homf = false;
    rec.ctrl.homr = false;

    rec.put_field("VAL", EpicsValue::Double(10.0)).unwrap();
    rec.plan_motion(CommandSource::Val);
    // Buttons stay at default; nothing surprising
    assert!(!rec.ctrl.homf);
    assert!(!rec.ctrl.homr);
}

// --- URIP RDBL error (C: db5da2f0 2017-05, 7493d50b 2018-04) ---

#[test]
fn test_urip_rdbl_error_blocks_new_move() {
    let mut rec = MotorRecord::new();
    rec.conv.urip = true;
    rec.set_rdbl_error(true);
    rec.pos.val = 0.0;

    rec.put_field("VAL", EpicsValue::Double(10.0)).unwrap();
    let effects = rec.plan_motion(CommandSource::Val);
    // No move commands emitted, motor stays Idle
    assert!(effects.commands.is_empty());
    assert_eq!(rec.stat.phase, MotionPhase::Idle);
}

#[test]
fn test_urip_rdbl_error_stops_in_flight_move_on_new_target() {
    let mut rec = MotorRecord::new();
    rec.conv.urip = true;
    rec.set_rdbl_error(false); // start healthy
    rec.put_field("VAL", EpicsValue::Double(10.0)).unwrap();
    rec.plan_motion(CommandSource::Val);
    assert_eq!(rec.stat.phase, MotionPhase::MainMove);

    // Now RDBL link errors. New write should trigger STOP.
    rec.set_rdbl_error(true);
    rec.put_field("VAL", EpicsValue::Double(20.0)).unwrap();
    let effects = rec.plan_motion(CommandSource::Val);
    assert!(
        effects
            .commands
            .iter()
            .any(|c| matches!(c, MotorCommand::Stop { .. }))
    );
}

#[test]
fn test_urip_rdbl_error_stops_in_flight_on_completion_check() {
    let mut rec = MotorRecord::new();
    rec.conv.urip = true;
    rec.put_field("VAL", EpicsValue::Double(10.0)).unwrap();
    rec.plan_motion(CommandSource::Val);
    assert_eq!(rec.stat.phase, MotionPhase::MainMove);

    // RDBL fails mid-motion. check_completion should emit STOP.
    rec.set_rdbl_error(true);
    let effects = rec.check_completion();
    assert!(
        effects
            .commands
            .iter()
            .any(|c| matches!(c, MotorCommand::Stop { .. }))
    );
}

#[test]
fn test_urip_rdbl_error_does_not_affect_ueip_path() {
    // When URIP=No, RDBL error flag is irrelevant.
    let mut rec = MotorRecord::new();
    rec.conv.urip = false;
    rec.set_rdbl_error(true);
    rec.put_field("VAL", EpicsValue::Double(10.0)).unwrap();
    let effects = rec.plan_motion(CommandSource::Val);
    assert!(!effects.commands.is_empty()); // move proceeds
}

// --- Inverted soft limits (C: 270347df PR #108) ---

#[test]
fn test_dllm_greater_than_dhlm_sets_lvio_immediately() {
    let mut rec = MotorRecord::new();
    // Sequence that produces an inverted dial pair via direct DHLM put.
    rec.put_field("DHLM", EpicsValue::Double(10.0)).unwrap();
    rec.put_field("DLLM", EpicsValue::Double(100.0)).unwrap();
    assert!(rec.limits.dllm > rec.limits.dhlm);
    assert!(rec.limits.lvio); // immediate, no poll required
}

#[test]
fn test_llm_greater_than_hlm_sets_lvio_immediately() {
    let mut rec = MotorRecord::new();
    // Each user-limit write moves one dial limit (C set_user_*limit), so
    // the inverted pair lands in BOTH frames and latches LVIO.
    rec.put_field("HLM", EpicsValue::Double(10.0)).unwrap();
    rec.put_field("LLM", EpicsValue::Double(100.0)).unwrap();
    assert!(rec.limits.llm > rec.limits.hlm);
    assert!(rec.limits.lvio);
}

#[test]
fn test_correcting_inverted_limits_clears_lvio_without_poll() {
    // C: 270347df — LVIO must clear when the limit pair becomes valid again,
    // even on an idle axis that is not being polled.
    let mut rec = MotorRecord::new();
    rec.conv.mres = 1.0;
    rec.pos.dval = 0.0; // inside any sane window
    // Create the inverted state.
    rec.put_field("DHLM", EpicsValue::Double(10.0)).unwrap();
    rec.put_field("DLLM", EpicsValue::Double(100.0)).unwrap();
    assert!(rec.limits.lvio);
    // Correct it: DLLM back below DHLM.
    rec.put_field("DLLM", EpicsValue::Double(-100.0)).unwrap();
    assert!(
        !rec.limits.lvio,
        "LVIO must clear once the limit pair is valid and DVAL is in range"
    );
}

#[test]
fn test_correcting_limits_keeps_lvio_when_position_still_outside() {
    let mut rec = MotorRecord::new();
    rec.conv.mres = 1.0;
    rec.pos.dval = 500.0; // far outside the corrected window
    rec.put_field("DHLM", EpicsValue::Double(10.0)).unwrap();
    rec.put_field("DLLM", EpicsValue::Double(-10.0)).unwrap();
    // Pair is valid now, but DVAL 500 is outside [-10, 10] → LVIO stays.
    assert!(rec.limits.lvio);
}

// --- MIP_EXTERNAL (C: ea063f5f, 2008-04) ---

#[test]
fn test_external_move_detected_when_driver_moves_while_idle() {
    use asyn_rs::interfaces::motor::MotorStatus;
    let mut rec = MotorRecord::new();
    // Record sits Idle, DMOV=true
    assert_eq!(rec.stat.phase, MotionPhase::Idle);
    assert!(rec.stat.dmov);

    let status = MotorStatus {
        position: 5.0,
        encoder_position: 5.0,
        velocity: 0.0,
        moving: true,
        done: false,
        direction: true,
        high_limit: false,
        low_limit: false,
        home: false,
        homed: false,
        powered: true,
        problem: false,
        slip_stall: false,
        comms_error: false,
        gain_support: true,
        has_encoder: false,
        vbas_supported: true,
    };
    rec.process_motor_info(&status);

    assert!(rec.stat.mip.contains(MipFlags::EXTERNAL));
    assert!(!rec.stat.dmov);
}

#[test]
fn test_external_move_idempotent_on_repeat_poll() {
    use asyn_rs::interfaces::motor::MotorStatus;
    let mut rec = MotorRecord::new();
    let status = MotorStatus {
        position: 5.0,
        encoder_position: 5.0,
        velocity: 0.0,
        moving: true,
        done: false,
        direction: true,
        high_limit: false,
        low_limit: false,
        home: false,
        homed: false,
        powered: true,
        problem: false,
        slip_stall: false,
        comms_error: false,
        gain_support: true,
        has_encoder: false,
        vbas_supported: true,
    };
    rec.process_motor_info(&status);
    assert!(rec.stat.mip.contains(MipFlags::EXTERNAL));
    // Second poll while still moving must not re-trigger external flagging
    // (the !mip.contains(EXTERNAL) guard makes it idempotent).
    rec.process_motor_info(&status);
    assert!(rec.stat.mip.contains(MipFlags::EXTERNAL));
}

#[test]
fn test_external_move_completion_syncs_and_clears_mip() {
    use asyn_rs::interfaces::motor::MotorStatus;
    let mut rec = MotorRecord::new();
    // Start: external move in progress
    let moving = MotorStatus {
        position: 0.0,
        encoder_position: 0.0,
        velocity: 0.0,
        moving: true,
        done: false,
        direction: true,
        high_limit: false,
        low_limit: false,
        home: false,
        homed: false,
        powered: true,
        problem: false,
        slip_stall: false,
        comms_error: false,
        gain_support: true,
        has_encoder: false,
        vbas_supported: true,
    };
    rec.process_motor_info(&moving);
    assert!(rec.stat.mip.contains(MipFlags::EXTERNAL));

    // Driver finishes the move at position 7.0 (integer to avoid MRES=1 rounding)
    let done = MotorStatus {
        position: 7.0,
        encoder_position: 7.0,
        velocity: 0.0,
        moving: false,
        done: true,
        direction: true,
        high_limit: false,
        low_limit: false,
        home: false,
        homed: false,
        powered: true,
        problem: false,
        slip_stall: false,
        comms_error: false,
        gain_support: true,
        has_encoder: false,
        vbas_supported: true,
    };
    // Drive the completion through the live dispatch path: process_motor_info
    // runs during the Idle-phase readback (determine_event reports no event),
    // and do_process() must still route MIP_EXTERNAL into check_completion.
    rec.process_motor_info(&done);
    let effects = rec.do_process();

    assert!(!rec.stat.mip.contains(MipFlags::EXTERNAL));
    assert!(rec.stat.dmov);
    assert_eq!(rec.pos.val, 7.0);
    assert_eq!(rec.pos.dval, 7.0);
    let _ = effects;
}

#[test]
fn test_external_move_in_progress_keeps_dmov_false_via_do_process() {
    use asyn_rs::interfaces::motor::MotorStatus;
    let mut rec = MotorRecord::new();
    let moving = MotorStatus {
        position: 3.0,
        encoder_position: 3.0,
        moving: true,
        done: false,
        ..Default::default()
    };
    rec.process_motor_info(&moving);
    // Still moving — do_process must not clear EXTERNAL or finalize.
    rec.process_motor_info(&moving);
    let _ = rec.do_process();
    assert!(rec.stat.mip.contains(MipFlags::EXTERNAL));
    assert!(!rec.stat.dmov);
}

#[test]
fn test_stop_during_external_move_emits_stop() {
    use asyn_rs::interfaces::motor::MotorStatus;
    let mut rec = MotorRecord::new();
    // External move flagged (phase stays Idle, MIP_EXTERNAL set).
    rec.process_motor_info(&MotorStatus {
        position: 3.0,
        moving: true,
        done: false,
        ..Default::default()
    });
    assert!(rec.stat.mip.contains(MipFlags::EXTERNAL));

    rec.put_field("STOP", EpicsValue::Short(1)).unwrap();
    let effects = rec.plan_motion(CommandSource::Stop);
    assert!(
        effects
            .commands
            .iter()
            .any(|c| matches!(c, MotorCommand::Stop { .. })),
        "STOP during an external move must emit a Stop command"
    );
}

#[test]
fn test_spmg_stop_during_external_move_emits_stop() {
    use asyn_rs::interfaces::motor::MotorStatus;
    let mut rec = MotorRecord::new();
    rec.process_motor_info(&MotorStatus {
        position: 3.0,
        moving: true,
        done: false,
        ..Default::default()
    });
    rec.ctrl.spmg = SpmgMode::Stop;
    let effects = rec.plan_motion(CommandSource::Spmg);
    assert!(
        effects
            .commands
            .iter()
            .any(|c| matches!(c, MotorCommand::Stop { .. })),
        "SPMG=Stop during an external move must emit a Stop command"
    );
}

// --- SYNC PV (C: 82c26005, 2010-04) ---

#[test]
fn test_sync_pv_reseeds_val_dval_rval_from_readback() {
    let mut rec = MotorRecord::new();
    rec.pos.drbv = 25.0;
    rec.pos.rrbv = 2500;
    rec.pos.rbv = 25.0;
    // VAL/DVAL/RVAL diverge from readback
    rec.pos.val = 0.0;
    rec.pos.dval = 0.0;
    rec.pos.rval = 0;

    rec.put_field("SYNC", EpicsValue::Short(1)).unwrap();
    rec.plan_motion(CommandSource::Sync);

    assert_eq!(rec.pos.val, 25.0);
    assert_eq!(rec.pos.dval, 25.0);
    assert_eq!(rec.pos.rval, 2500);
}

#[test]
fn test_sync_pv_get_reflects_latch() {
    let mut rec = MotorRecord::new();
    rec.put_field("SYNC", EpicsValue::Short(1)).unwrap();
    // C pmr->sync is a real field: it reads 1 while the request is
    // latched (put happened, record not yet processed)...
    assert_eq!(rec.get_field("SYNC"), Some(EpicsValue::Short(1)));
    // ...and posts back to 0 once the idle-gated apply consumes it
    // (motorRecord.cc:2542-2543).
    rec.plan_motion(CommandSource::Sync);
    assert_eq!(rec.get_field("SYNC"), Some(EpicsValue::Short(0)));
}

// --- RSTM init integration (C: devMotorAsyn.c init_controller) ---

#[test]
fn test_rstm_never_always_syncs_to_driver() {
    use asyn_rs::interfaces::motor::MotorStatus;
    let mut rec = MotorRecord::new();
    rec.conv.rstm = RestoreMode::Never;
    rec.conv.mres = 1.0;
    rec.pos.dval = 7.0; // autosaved
    let status = MotorStatus {
        position: 0.0, // driver lost position
        encoder_position: 0.0,
        done: true,
        ..Default::default()
    };
    let effects = rec.initial_readback(&status);
    // Never restore → adopt driver readback (0), no SetPosition emitted
    assert_eq!(rec.pos.dval, 0.0);
    assert!(
        !effects
            .commands
            .iter()
            .any(|c| matches!(c, MotorCommand::SetPosition { .. }))
    );
}

#[test]
fn test_rstm_always_restores_autosaved_dval() {
    use asyn_rs::interfaces::motor::MotorStatus;
    let mut rec = MotorRecord::new();
    rec.conv.rstm = RestoreMode::Always;
    rec.conv.mres = 1.0;
    rec.pos.dval = 7.0; // autosaved
    let status = MotorStatus {
        position: 0.0,
        encoder_position: 0.0,
        done: true,
        ..Default::default()
    };
    let effects = rec.initial_readback(&status);
    // Always → keep autosaved DVAL, push SetPosition(7.0)
    assert_eq!(rec.pos.dval, 7.0);
    assert!(effects.commands.iter().any(
        |c| matches!(c, MotorCommand::SetPosition { position } if (*position - 7.0).abs() < 1e-9)
    ));
}

#[test]
fn test_rstm_near_zero_restores_when_driver_near_zero() {
    use asyn_rs::interfaces::motor::MotorStatus;
    let mut rec = MotorRecord::new();
    rec.conv.rstm = RestoreMode::NearZero;
    rec.conv.mres = 0.01;
    rec.retry.rdbd = 0.05;
    rec.pos.dval = 5.0; // autosaved, |dval| > rdbd
    let status = MotorStatus {
        position: 0.0, // driver near zero
        encoder_position: 0.0,
        done: true,
        ..Default::default()
    };
    let effects = rec.initial_readback(&status);
    assert_eq!(rec.pos.dval, 5.0);
    assert!(
        effects
            .commands
            .iter()
            .any(|c| matches!(c, MotorCommand::SetPosition { .. }))
    );
}

#[test]
fn test_rstm_near_zero_no_restore_when_driver_already_positioned() {
    use asyn_rs::interfaces::motor::MotorStatus;
    let mut rec = MotorRecord::new();
    rec.conv.rstm = RestoreMode::NearZero;
    rec.conv.mres = 1.0;
    rec.retry.rdbd = 0.05;
    // Autosaved DVAL and driver position are deliberately different (5 vs 8)
    // so restore (dval stays 5) and sync (dval becomes 8) are distinguishable.
    rec.pos.dval = 5.0;
    let status = MotorStatus {
        position: 8.0, // driver at 8.0 — far from zero
        encoder_position: 8.0,
        done: true,
        ..Default::default()
    };
    let effects = rec.initial_readback(&status);
    // Driver position is meaningful → no restore → adopt driver readback (8.0).
    assert_eq!(
        rec.pos.dval, 8.0,
        "should sync to driver position, not restore"
    );
    assert!(
        !effects
            .commands
            .iter()
            .any(|c| matches!(c, MotorCommand::SetPosition { .. }))
    );
}

#[test]
fn test_rstm_near_zero_uses_motor_position_not_encoder() {
    use asyn_rs::interfaces::motor::MotorStatus;
    // C compares the *motor* dial (status.position * MRES), not the encoder
    // dial. With UEIP=Yes and ERES != MRES, the encoder readback differs from
    // the motor position; the near-zero decision must follow the motor.
    let mut rec = MotorRecord::new();
    rec.conv.rstm = RestoreMode::NearZero;
    rec.conv.mres = 1.0;
    rec.conv.eres = 10.0; // encoder scale differs
    rec.conv.ueip = true;
    rec.retry.rdbd = 0.05;
    rec.pos.dval = 5.0; // autosaved, meaningful
    let status = MotorStatus {
        position: 0.0,         // motor at zero → restore expected
        encoder_position: 5.0, // encoder reads non-zero
        done: true,
        ..Default::default()
    };
    let effects = rec.initial_readback(&status);
    // Motor position is near zero → restore the autosaved DVAL.
    assert_eq!(rec.pos.dval, 5.0);
    assert!(
        effects
            .commands
            .iter()
            .any(|c| matches!(c, MotorCommand::SetPosition { .. }))
    );
}

// --- LOAD_POS block (epics-modules/motor #231) ---

#[test]
fn test_loadpos_block_refuses_set_mode_redefinition() {
    let mut rec = MotorRecord::new();
    rec.conv.mres = 0.01;
    rec.conv.set = true;
    rec.conv.loadpos_blocked = true;
    rec.pos.dval = 5.0;
    rec.pos.off = 0.0;

    // SET-mode VAL write would normally redefine OFF/DVAL — must be refused.
    rec.put_field("VAL", EpicsValue::Double(100.0)).unwrap();
    assert_eq!(rec.pos.dval, 5.0); // unchanged
    assert_eq!(rec.pos.off, 0.0); // unchanged — DVAL/OFF stay consistent
}

#[test]
fn test_loadpos_block_allows_normal_set_when_unblocked() {
    let mut rec = MotorRecord::new();
    rec.conv.mres = 0.01;
    rec.conv.set = true;
    rec.conv.loadpos_blocked = false;
    rec.pos.dval = 5.0;
    rec.put_field("VAL", EpicsValue::Double(100.0)).unwrap();
    // Unblocked: SET mode redefines OFF (100 - 1*5 = 95)
    assert_eq!(rec.pos.off, 95.0);
}

#[test]
fn test_loadpos_block_skips_rstm_restore() {
    use asyn_rs::interfaces::motor::MotorStatus;
    let mut rec = MotorRecord::new();
    rec.conv.rstm = RestoreMode::Always;
    rec.conv.mres = 1.0;
    rec.conv.loadpos_blocked = true;
    rec.pos.dval = 7.0;
    let status = MotorStatus {
        position: 0.0,
        done: true,
        ..Default::default()
    };
    let effects = rec.initial_readback(&status);
    // LOAD_POS blocked → no SetPosition, driver readback adopted
    assert_eq!(rec.pos.dval, 0.0);
    assert!(
        !effects
            .commands
            .iter()
            .any(|c| matches!(c, MotorCommand::SetPosition { .. }))
    );
}

#[test]
fn test_loadpos_block_field_roundtrip() {
    let mut rec = MotorRecord::new();
    rec.put_field("LOADPOS_BLOCK", EpicsValue::Short(1))
        .unwrap();
    assert_eq!(rec.get_field("LOADPOS_BLOCK"), Some(EpicsValue::Short(1)));
    assert!(rec.conv.loadpos_blocked);
}

// --- RSTM autosaved MRES mismatch interlock (epics-modules/motor #196) ---

#[test]
fn test_rstm_restore_skipped_on_mres_mismatch() {
    use asyn_rs::interfaces::motor::MotorStatus;
    let mut rec = MotorRecord::new();
    rec.conv.rstm = RestoreMode::Always;
    rec.conv.mres = 0.02; // changed since autosave
    rec.retry.rdbd = 0.05;
    // Autosaved DVAL=5.0 and RVAL=500 were consistent at MRES=0.01
    // (500 * 0.01 = 5.0), but at the current MRES 500 * 0.02 = 10.0.
    rec.pos.dval = 5.0;
    rec.pos.rval = 500;
    let status = MotorStatus {
        position: 0.0,
        encoder_position: 0.0,
        done: true,
        ..Default::default()
    };
    let effects = rec.initial_readback(&status);
    // Inconsistent autosave → restore skipped, driver readback adopted
    assert_eq!(rec.pos.dval, 0.0);
    assert!(
        !effects
            .commands
            .iter()
            .any(|c| matches!(c, MotorCommand::SetPosition { .. }))
    );
}

#[test]
fn test_rstm_restore_proceeds_when_mres_consistent() {
    use asyn_rs::interfaces::motor::MotorStatus;
    let mut rec = MotorRecord::new();
    rec.conv.rstm = RestoreMode::Always;
    rec.conv.mres = 0.01;
    rec.retry.rdbd = 0.05;
    // DVAL=5.0, RVAL=500 — consistent at MRES=0.01 (500 * 0.01 = 5.0)
    rec.pos.dval = 5.0;
    rec.pos.rval = 500;
    let status = MotorStatus {
        position: 0.0,
        encoder_position: 0.0,
        done: true,
        ..Default::default()
    };
    let effects = rec.initial_readback(&status);
    assert_eq!(rec.pos.dval, 5.0);
    assert!(
        effects
            .commands
            .iter()
            .any(|c| matches!(c, MotorCommand::SetPosition { .. }))
    );
}

// --- RHLM / RLLM (C: 2e89b552 PR #193, fd808eb2 PR #206) ---

#[test]
fn test_rhlm_rllm_roundtrip_via_pv() {
    let mut rec = MotorRecord::new();
    rec.conv.mres = 0.001;
    rec.put_field("RHLM", EpicsValue::Double(10_000.0)).unwrap();
    rec.put_field("RLLM", EpicsValue::Double(-10_000.0))
        .unwrap();
    assert_eq!(rec.get_field("RHLM"), Some(EpicsValue::Double(10_000.0)));
    assert_eq!(rec.get_field("RLLM"), Some(EpicsValue::Double(-10_000.0)));
    // Dial limits derived from raw * mres
    assert!((rec.limits.dhlm - 10.0).abs() < 1e-9);
    assert!((rec.limits.dllm + 10.0).abs() < 1e-9);
}

#[test]
fn test_mres_change_preserves_raw_limits() {
    let mut rec = MotorRecord::new();
    rec.conv.mres = 0.01;
    rec.put_field("DHLM", EpicsValue::Double(100.0)).unwrap();
    rec.put_field("DLLM", EpicsValue::Double(-100.0)).unwrap();
    // Raw limits set from dial on first DHLM/DLLM put
    assert!((rec.limits.rhlm - 10_000.0).abs() < 1e-6);
    assert!((rec.limits.rllm + 10_000.0).abs() < 1e-6);

    // init_record establishes the limit invariant — only after this does a
    // runtime MRES change keep RHLM/RLLM invariant and rescale DHLM/DLLM.
    rec.init_record(1).unwrap();

    // Change MRES: raw stays, dial rescales
    rec.put_field("MRES", EpicsValue::Double(0.001)).unwrap();
    assert!((rec.limits.rhlm - 10_000.0).abs() < 1e-6);
    assert!((rec.limits.rllm + 10_000.0).abs() < 1e-6);
    assert!((rec.limits.dhlm - 10.0).abs() < 1e-9);
    assert!((rec.limits.dllm + 10.0).abs() < 1e-9);
}

#[test]
fn test_negative_mres_swaps_dial_limit_pair() {
    let mut rec = MotorRecord::new();
    rec.conv.mres = 1.0;
    rec.put_field("DHLM", EpicsValue::Double(100.0)).unwrap();
    rec.put_field("DLLM", EpicsValue::Double(-100.0)).unwrap();
    rec.init_record(1).unwrap();
    // Now switch MRES to negative
    rec.put_field("MRES", EpicsValue::Double(-1.0)).unwrap();
    // C: fd808eb2 — dial pair must keep DHLM >= DLLM
    assert!(rec.limits.dhlm >= rec.limits.dllm);
}

#[test]
fn test_template_load_dhlm_before_mres_preserves_egu_limits() {
    // Reproduces the standard motor.template field order — field(DHLM),
    // field(DLLM) then field(MRES) — all applied through put_field by
    // dbLoadRecords::apply_fields, followed by init_record. The DHLM=100
    // engineering-unit limit must survive: it must NOT be rescaled to 0.1
    // by the MRES cascade running mid-load (regression: ophyd EpicsMotor
    // saw ctrl limits [-0.1, 0.1] and rejected every real scan).
    let mut rec = MotorRecord::new();
    rec.put_field("DHLM", EpicsValue::Double(100.0)).unwrap();
    rec.put_field("DLLM", EpicsValue::Double(-100.0)).unwrap();
    rec.put_field("MRES", EpicsValue::Double(0.001)).unwrap();
    rec.init_record(0).unwrap();
    rec.init_record(1).unwrap();

    assert!(
        (rec.limits.dhlm - 100.0).abs() < 1e-9,
        "DHLM={}",
        rec.limits.dhlm
    );
    assert!(
        (rec.limits.dllm + 100.0).abs() < 1e-9,
        "DLLM={}",
        rec.limits.dllm
    );
    assert!(
        (rec.limits.hlm - 100.0).abs() < 1e-9,
        "HLM={}",
        rec.limits.hlm
    );
    assert!(
        (rec.limits.llm + 100.0).abs() < 1e-9,
        "LLM={}",
        rec.limits.llm
    );
    // Raw limits derived from the dial limits at the final MRES.
    assert!(
        (rec.limits.rhlm - 100_000.0).abs() < 1e-3,
        "RHLM={}",
        rec.limits.rhlm
    );
    assert!(
        (rec.limits.rllm + 100_000.0).abs() < 1e-3,
        "RLLM={}",
        rec.limits.rllm
    );
}

// --- Speed invariant at init (C check_speed_and_resolution, motorRecord.cc:3895) ---

#[test]
fn test_template_load_velo_before_mres_preserves_velo() {
    // Reproduces the standard motor.template field order — field(VELO),
    // field(ACCL) then field(MRES) — all applied through put_field by
    // dbLoadRecords::apply_fields, followed by init_record. VELO=50 must
    // survive: it must NOT be rewritten to UREV×S = 0.2×50 = 10 by the MRES
    // cascade consuming the S that the VELO put derived against the default
    // UREV (regression: every motor.template VELO came up scaled 0.2×).
    let mut rec = MotorRecord::new();
    rec.put_field("VELO", EpicsValue::Double(50.0)).unwrap();
    rec.put_field("ACCL", EpicsValue::Double(0.2)).unwrap();
    rec.put_field("MRES", EpicsValue::Double(0.001)).unwrap();
    rec.init_record(0).unwrap();
    rec.init_record(1).unwrap();

    assert!((rec.vel.velo - 50.0).abs() < 1e-9, "VELO={}", rec.vel.velo);
    // S derived from VELO at the final UREV = 0.001 × 200 = 0.2
    assert!((rec.vel.s - 250.0).abs() < 1e-9, "S={}", rec.vel.s);
    assert!(
        (rec.conv.urev - 0.2).abs() < 1e-12,
        "UREV={}",
        rec.conv.urev
    );
    // ACCU=Accl master: ACCS reconciled from the surviving VELO
    assert!((rec.vel.accs - 250.0).abs() < 1e-9, "ACCS={}", rec.vel.accs);
}

#[test]
fn test_load_velo_clamped_to_vmax_at_init() {
    // C check_speed_and_resolution range_check: VELO is clamped into
    // [VBAS, VMAX] at init (max enforced only when nonzero).
    let mut rec = MotorRecord::new();
    rec.put_field("VELO", EpicsValue::Double(50.0)).unwrap();
    rec.put_field("VMAX", EpicsValue::Double(20.0)).unwrap();
    rec.put_field("MRES", EpicsValue::Double(0.001)).unwrap();
    rec.init_record(0).unwrap();
    rec.init_record(1).unwrap();

    assert!((rec.vel.vmax - 20.0).abs() < 1e-9, "VMAX={}", rec.vel.vmax);
    assert!((rec.vel.smax - 100.0).abs() < 1e-9, "SMAX={}", rec.vel.smax);
    assert!((rec.vel.velo - 20.0).abs() < 1e-9, "VELO={}", rec.vel.velo);
    assert!((rec.vel.s - 100.0).abs() < 1e-9, "S={}", rec.vel.s);
}

#[test]
fn test_load_s_specified_wins_over_velo_at_init() {
    // C: a nonzero S at init means the .db specified it — S wins and VELO
    // is derived (motorRecord.cc:3955-3958), even if VELO was also loaded.
    let mut rec = MotorRecord::new();
    rec.put_field("VELO", EpicsValue::Double(50.0)).unwrap();
    rec.put_field("S", EpicsValue::Double(2.5)).unwrap();
    rec.put_field("MRES", EpicsValue::Double(0.001)).unwrap();
    rec.init_record(0).unwrap();
    rec.init_record(1).unwrap();

    assert!((rec.vel.s - 2.5).abs() < 1e-9, "S={}", rec.vel.s);
    assert!((rec.vel.velo - 0.5).abs() < 1e-9, "VELO={}", rec.vel.velo);
}

#[test]
fn test_runtime_velo_s_puts_cross_calc_after_init() {
    // After init the C special() semantics apply: a VELO put re-derives S
    // and an S put re-derives VELO at the current UREV.
    let mut rec = MotorRecord::new();
    rec.put_field("MRES", EpicsValue::Double(0.001)).unwrap();
    rec.init_record(0).unwrap();
    rec.init_record(1).unwrap();

    rec.put_field("VELO", EpicsValue::Double(10.0)).unwrap();
    assert!((rec.vel.s - 50.0).abs() < 1e-9, "S={}", rec.vel.s);
    rec.put_field("S", EpicsValue::Double(100.0)).unwrap();
    assert!((rec.vel.velo - 20.0).abs() < 1e-9, "VELO={}", rec.vel.velo);
}

#[test]
fn test_runtime_mres_change_rederives_velo_from_s() {
    // C velcheckB (motorRecord.cc:2855-2875): a runtime resolution change
    // keeps the rev-unit speeds invariant and re-derives the EGU speeds.
    let mut rec = MotorRecord::new();
    rec.put_field("VELO", EpicsValue::Double(50.0)).unwrap();
    rec.put_field("MRES", EpicsValue::Double(0.001)).unwrap();
    rec.init_record(0).unwrap();
    rec.init_record(1).unwrap();
    assert!((rec.vel.s - 250.0).abs() < 1e-9, "S={}", rec.vel.s);

    rec.put_field("MRES", EpicsValue::Double(0.002)).unwrap();
    // UREV = 0.002 × 200 = 0.4; VELO = UREV × S = 0.4 × 250 = 100
    assert!(
        (rec.conv.urev - 0.4).abs() < 1e-12,
        "UREV={}",
        rec.conv.urev
    );
    assert!((rec.vel.s - 250.0).abs() < 1e-9, "S={}", rec.vel.s);
    assert!((rec.vel.velo - 100.0).abs() < 1e-9, "VELO={}", rec.vel.velo);
}

// --- RSTM (C: 2906f3d8, PR #160) ---

#[test]
fn test_rstm_default_is_near_zero() {
    let rec = MotorRecord::new();
    assert_eq!(rec.conv.rstm, RestoreMode::NearZero);
}

#[test]
fn test_rstm_roundtrip() {
    let mut rec = MotorRecord::new();
    rec.put_field("RSTM", EpicsValue::Short(1)).unwrap();
    assert_eq!(rec.conv.rstm, RestoreMode::Always);
    assert_eq!(rec.get_field("RSTM"), Some(EpicsValue::Short(1)));
    rec.put_field("RSTM", EpicsValue::Short(0)).unwrap();
    assert_eq!(rec.conv.rstm, RestoreMode::Never);
    rec.put_field("RSTM", EpicsValue::Short(3)).unwrap();
    assert_eq!(rec.conv.rstm, RestoreMode::Conditional);
}

#[test]
fn test_rstm_should_restore_dispatch() {
    // Never: never restore
    assert!(!RestoreMode::Never.should_restore(false, false));
    assert!(!RestoreMode::Never.should_restore(true, true));
    // Always: always restore
    assert!(RestoreMode::Always.should_restore(false, false));
    // NearZero: only when DVAL non-zero AND position near zero
    assert!(RestoreMode::NearZero.should_restore(false, true));
    assert!(!RestoreMode::NearZero.should_restore(true, false));
    // Conditional: use_rel OR DVAL-non-zero-near-zero
    assert!(RestoreMode::Conditional.should_restore(true, false));
    assert!(RestoreMode::Conditional.should_restore(false, true));
    assert!(!RestoreMode::Conditional.should_restore(false, false));
}

#[test]
fn test_accs_roundtrip_via_pv() {
    let mut rec = MotorRecord::new();
    rec.vel.velo = 8.0;
    rec.vel.vbas = 0.0;
    rec.put_field("ACCS", EpicsValue::Double(4.0)).unwrap();
    assert_eq!(rec.get_field("ACCS"), Some(EpicsValue::Double(4.0)));
    assert_eq!(rec.get_field("ACCU"), Some(EpicsValue::Short(1)));
}

// --- MDEL/ADEL deadband target (C: monitor() gates on RBV) ---

#[test]
fn test_monitor_deadband_value_is_rbv_not_val() {
    // The harness applies MDEL/ADEL to monitor_deadband_value(). For a motor
    // record that must be the readback (RBV), not the VAL setpoint.
    let mut rec = MotorRecord::new();
    rec.pos.rbv = 42.0;
    rec.pos.val = 10.0; // setpoint differs from readback
    assert_eq!(rec.monitor_deadband_value(), Some(EpicsValue::Double(42.0)));
}

// --- Pause/Go resume semantics (C pp + maybeRetry, motorRecord.cc
// 1383-1402, 1040-1100, 2241) ---

/// Shared setup: move 0 → 50, axis reported moving.
fn paused_move_setup() -> MotorRecord {
    let mut rec = MotorRecord::new();
    rec.put_field("HLM", EpicsValue::Double(1000.0)).unwrap();
    rec.put_field("LLM", EpicsValue::Double(-1000.0)).unwrap();
    rec.put_field("VAL", EpicsValue::Double(50.0)).unwrap();
    rec.plan_motion(CommandSource::Val);
    rec.stat.msta = MstaFlags::MOVING;
    rec.stat.movn = true;
    rec
}

/// Drive the axis to a stop at `pos` and run the completion pass.
fn complete_stop_at(rec: &mut MotorRecord, pos: f64) -> ProcessEffects {
    rec.pos.drbv = pos;
    rec.pos.rbv = pos;
    rec.pos.rrbv = pos as i64;
    rec.stat.msta = MstaFlags::DONE;
    rec.stat.movn = false;
    rec.check_completion()
}

#[test]
fn test_pause_resume_targets_value_written_during_pause() {
    // C: a position write while paused is collected into VAL/DVAL but not
    // acted on (do_work returns at 2237 before the move block); the Go
    // pass move block then fires toward the freshest DVAL (2241).
    let mut rec = paused_move_setup();
    rec.ctrl.spmg = SpmgMode::Pause;
    rec.plan_motion(CommandSource::Spmg);
    complete_stop_at(&mut rec, 25.0);
    assert_eq!(rec.stat.mip, MipFlags::RETRY, "paused resume armed");

    rec.put_field("VAL", EpicsValue::Double(80.0)).unwrap();
    let effects = rec.plan_motion(CommandSource::Val);
    assert!(effects.commands.is_empty(), "no dispatch while paused");

    rec.ctrl.spmg = SpmgMode::Go;
    let effects = rec.plan_motion(CommandSource::Spmg);
    assert!(
        effects.commands.iter().any(|c| matches!(
            c,
            MotorCommand::MoveAbsolute { position, .. } if (position - 80.0).abs() < 1e-9
        )),
        "Go resumes toward the target written during the pause"
    );
}

#[test]
fn test_pause_stop_close_to_target_finalizes_without_resume() {
    // Boundary |dval - drbv| < RDBD: C maybeRetry close-enough path
    // (1084-1099) collapses to DONE — DMOV posts, nothing armed.
    let mut rec = paused_move_setup();
    rec.ctrl.spmg = SpmgMode::Pause;
    rec.plan_motion(CommandSource::Spmg);
    // RDBD is enforced to >= |MRES| = 1.0; 0.5 is inside the deadband.
    complete_stop_at(&mut rec, 49.5);
    assert!(rec.stat.dmov);
    assert_eq!(rec.stat.mip, MipFlags::empty());

    rec.ctrl.spmg = SpmgMode::Go;
    let effects = rec.plan_motion(CommandSource::Spmg);
    assert!(effects.commands.is_empty(), "nothing to resume");
}

#[test]
fn test_pause_stop_with_rtry_zero_does_not_arm_resume() {
    // C maybeRetry RTRY==0 (1052-1055): clear MIP, no retry arming. The
    // drive fields keep the target (no postProcess ran) but Go finds
    // dval == ldvl with DMOV true and fires nothing.
    let mut rec = paused_move_setup();
    rec.retry.rtry = 0;
    rec.ctrl.spmg = SpmgMode::Pause;
    rec.plan_motion(CommandSource::Spmg);
    complete_stop_at(&mut rec, 25.0);
    assert!(rec.stat.dmov);
    assert_eq!(rec.stat.mip, MipFlags::empty());
    assert_eq!(rec.pos.dval, 50.0, "C: target not synced away either");

    rec.ctrl.spmg = SpmgMode::Go;
    let effects = rec.plan_motion(CommandSource::Spmg);
    assert!(effects.commands.is_empty(), "RTRY=0 has no paused resume");
}

#[test]
fn test_pause_stop_retry_exhaustion_latches_miss() {
    // Boundary ++rcnt > rtry at the pause stop: C 1060-1075 gives up —
    // MIP_DONE, MISS latched, lasts adopt the unreached target.
    let mut rec = paused_move_setup();
    rec.retry.rtry = 2;
    rec.retry.rcnt = 2; // two paused stops already counted
    rec.ctrl.spmg = SpmgMode::Pause;
    rec.plan_motion(CommandSource::Spmg);
    complete_stop_at(&mut rec, 25.0);
    assert!(rec.stat.dmov);
    assert_eq!(rec.stat.mip, MipFlags::empty());
    assert!(rec.retry.miss, "MISS latches on retry exhaustion");

    rec.ctrl.spmg = SpmgMode::Go;
    let effects = rec.plan_motion(CommandSource::Spmg);
    assert!(
        effects.commands.is_empty(),
        "exhausted retry does not resume"
    );
}

#[test]
fn test_stop_during_pause_defuses_resume() {
    // C top-block stop branch (1874-1888): a STOP pulse on the armed
    // MIP_RETRY collapses it to DONE with DMOV=TRUE — Go must not resume.
    let mut rec = paused_move_setup();
    rec.ctrl.spmg = SpmgMode::Pause;
    rec.plan_motion(CommandSource::Spmg);
    complete_stop_at(&mut rec, 25.0);
    assert_eq!(rec.stat.mip, MipFlags::RETRY);

    rec.ctrl.stop = true;
    let effects = rec.plan_motion(CommandSource::Stop);
    assert!(matches!(effects.commands[0], MotorCommand::Stop { .. }));
    assert!(rec.stat.dmov);
    assert_eq!(rec.stat.mip, MipFlags::empty());

    rec.ctrl.spmg = SpmgMode::Go;
    let effects = rec.plan_motion(CommandSource::Spmg);
    assert!(
        effects.commands.is_empty(),
        "STOP defused the paused resume"
    );
}

#[test]
fn test_target_written_under_spmg_stop_dispatches_on_go() {
    // C: a position write under SPMG=Stop is collected (2205-2235) but the
    // pass returns before the move block (2237); the Go pass fires it via
    // `dval != ldvl` (2241).
    let mut rec = paused_move_setup();
    rec.ctrl.spmg = SpmgMode::Stop;
    rec.plan_motion(CommandSource::Spmg);
    complete_stop_at(&mut rec, 25.0);
    assert!(rec.stat.dmov);
    assert_eq!(rec.pos.dval, 25.0, "commanded stop syncs (pp armed)");

    rec.put_field("VAL", EpicsValue::Double(80.0)).unwrap();
    let effects = rec.plan_motion(CommandSource::Val);
    assert!(effects.commands.is_empty(), "no dispatch under SPMG=Stop");

    rec.ctrl.spmg = SpmgMode::Go;
    let effects = rec.plan_motion(CommandSource::Spmg);
    assert!(
        effects.commands.iter().any(|c| matches!(
            c,
            MotorCommand::MoveAbsolute { position, .. } if (position - 80.0).abs() < 1e-9
        )),
        "Go dispatches the target collected under SPMG=Stop"
    );
}

#[test]
fn test_pause_during_active_jog_syncs_and_button_resumes_jog() {
    // C: the jog dispatch armed pp (2125), so the pause-stop completion
    // post-processes (sync) instead of arming a move resume; the
    // still-latched button resumes the JOG on Go (1916-1918).
    let mut rec = MotorRecord::new();
    rec.put_field("HLM", EpicsValue::Double(1000.0)).unwrap();
    rec.put_field("LLM", EpicsValue::Double(-1000.0)).unwrap();
    rec.ctrl.jogf = true;
    rec.plan_motion(CommandSource::Jogf);
    assert_eq!(rec.stat.phase, MotionPhase::Jog);
    rec.stat.msta = MstaFlags::MOVING;
    rec.stat.movn = true;

    rec.ctrl.spmg = SpmgMode::Pause;
    rec.plan_motion(CommandSource::Spmg);
    complete_stop_at(&mut rec, 25.0);
    assert!(rec.stat.dmov);
    assert_eq!(rec.pos.dval, 25.0, "interrupted jog syncs at the stop");
    assert!(rec.ctrl.jogf, "jog button survives the pause");

    rec.ctrl.spmg = SpmgMode::Go;
    let effects = rec.plan_motion(CommandSource::Spmg);
    assert!(
        effects.commands.iter().any(|c| matches!(
            c,
            MotorCommand::MoveVelocity {
                direction: true,
                ..
            }
        )),
        "Go resumes the jog, not a position move"
    );
    assert!(
        !effects
            .commands
            .iter()
            .any(|c| matches!(c, MotorCommand::MoveAbsolute { .. })),
        "no move-block resume after the jog sync"
    );
}

#[test]
fn test_pause_during_backlash_move_syncs_at_completion() {
    // C 2523: a move dispatched with a pending backlash correction arms pp
    // ("do backlash from postprocess()"), so a Pause syncs at the stop
    // completion instead of arming the paused resume.
    let mut rec = MotorRecord::new();
    rec.put_field("HLM", EpicsValue::Double(1000.0)).unwrap();
    rec.put_field("LLM", EpicsValue::Double(-1000.0)).unwrap();
    rec.retry.bdst = -5.0; // forward move opposes BDST → backlash arming
    rec.put_field("VAL", EpicsValue::Double(50.0)).unwrap();
    rec.plan_motion(CommandSource::Val);
    assert!(rec.internal.backlash_pending);
    rec.stat.msta = MstaFlags::MOVING;
    rec.stat.movn = true;

    rec.ctrl.spmg = SpmgMode::Pause;
    rec.plan_motion(CommandSource::Spmg);
    complete_stop_at(&mut rec, 25.0);
    assert!(rec.stat.dmov);
    assert_eq!(rec.stat.mip, MipFlags::empty());
    assert_eq!(rec.pos.dval, 25.0, "backlash-armed move syncs on pause");

    rec.ctrl.spmg = SpmgMode::Go;
    let effects = rec.plan_motion(CommandSource::Spmg);
    assert!(effects.commands.is_empty());
}

#[test]
fn test_idle_pause_sends_stop_axis_and_converges() {
    // C 1899-1911: the Pause pass sends STOP_AXIS unconditionally and
    // sets mip = MIP_STOP even on an idle axis ("just in case" — e.g. a
    // servo still settling after an InPosition retry). The next
    // completion pass collapses the bare MIP_STOP via maybeRetry's
    // close-enough path (mip != DONE at 1432) without arming DLY
    // (C 1457 needs a fresh M_DMOV mark).
    let mut rec = MotorRecord::new();
    rec.timing.dly = 1.0;
    rec.ctrl.spmg = SpmgMode::Pause;
    let effects = rec.plan_motion(CommandSource::Spmg);
    assert!(matches!(effects.commands[0], MotorCommand::Stop { .. }));
    assert_eq!(rec.stat.mip, MipFlags::STOP);
    assert!(rec.stat.dmov);

    rec.stat.msta = MstaFlags::DONE;
    let effects = rec.check_completion();
    assert!(effects.commands.is_empty());
    assert!(
        effects.schedule_delay.is_none(),
        "idle pause must not arm DLY"
    );
    assert_eq!(rec.stat.mip, MipFlags::empty());
    assert!(rec.stat.dmov);

    rec.ctrl.spmg = SpmgMode::Go;
    let effects = rec.plan_motion(CommandSource::Spmg);
    assert!(
        effects.commands.is_empty(),
        "nothing to resume after idle pause"
    );
}

// --- too_small quiesce parity (C 2308-2348) ---

#[test]
fn test_sub_step_move_quiesces_lasts() {
    // C 2343-2347: the too-small suppress path updates the previous-target
    // registers so the divergence is not re-detected on later passes.
    let mut rec = MotorRecord::new();
    rec.put_field("HLM", EpicsValue::Double(1000.0)).unwrap();
    rec.put_field("LLM", EpicsValue::Double(-1000.0)).unwrap();
    rec.put_field("VAL", EpicsValue::Double(0.4)).unwrap();
    let effects = rec.plan_motion(CommandSource::Val);
    assert!(effects.commands.is_empty(), "sub-step move suppressed");
    assert_eq!(rec.internal.lval, 0.4);
    assert_eq!(rec.internal.ldvl, 0.4);
}

#[test]
fn test_spdb_window_quiesces_lasts() {
    // C 2317-2326: SPDB folds into the too_small decision (open window
    // around DRBV) and shares the same quiesce.
    let mut rec = MotorRecord::new();
    rec.put_field("HLM", EpicsValue::Double(1000.0)).unwrap();
    rec.put_field("LLM", EpicsValue::Double(-1000.0)).unwrap();
    rec.conv.mres = 0.001;
    rec.retry.spdb = 2.0;
    rec.put_field("VAL", EpicsValue::Double(1.5)).unwrap();
    let effects = rec.plan_motion(CommandSource::Val);
    assert!(effects.commands.is_empty(), "SPDB window suppressed");
    assert_eq!(rec.internal.ldvl, 1.5);
    assert_eq!(rec.internal.lval, 1.5);
}

#[test]
fn test_retry_resume_within_deadband_quiesces() {
    // C 2329-2342: a RETRY dispatch compares against RDBD in steps; the
    // paused resume completes without moving (mip -> DONE) when the axis
    // drifted inside the deadband during the pause.
    let mut rec = paused_move_setup();
    rec.retry.rdbd = 5.0;
    rec.ctrl.spmg = SpmgMode::Pause;
    rec.plan_motion(CommandSource::Spmg);
    complete_stop_at(&mut rec, 25.0);
    assert_eq!(rec.stat.mip, MipFlags::RETRY);

    // Axis drifts into the retry deadband while paused.
    rec.pos.drbv = 47.0;
    rec.pos.rbv = 47.0;

    rec.ctrl.spmg = SpmgMode::Go;
    let effects = rec.plan_motion(CommandSource::Spmg);
    assert!(effects.commands.is_empty(), "within RDBD: no resume move");
    assert_eq!(rec.stat.mip, MipFlags::empty());
    assert_eq!(rec.internal.ldvl, 50.0, "quiesced against the target");
}

// --- Overtaken-target replay (C 1383-1397) ---

#[test]
fn test_val_written_during_home_replays_at_completion() {
    // A VAL written while homing is ignored by the dispatcher
    // (handle_retarget: HOMF is not in-move). C replays it at the home
    // completion (1387-1397) instead of syncing it away.
    let mut rec = MotorRecord::new();
    rec.put_field("HLM", EpicsValue::Double(1000.0)).unwrap();
    rec.put_field("LLM", EpicsValue::Double(-1000.0)).unwrap();
    rec.ctrl.homf = true;
    rec.plan_motion(CommandSource::Homf);
    assert_eq!(rec.stat.phase, MotionPhase::Homing);
    rec.stat.msta = MstaFlags::MOVING;
    rec.stat.movn = true;

    rec.put_field("VAL", EpicsValue::Double(80.0)).unwrap();
    let effects = rec.plan_motion(CommandSource::Val);
    assert!(effects.commands.is_empty(), "no dispatch while homing");

    let effects = complete_stop_at(&mut rec, 5.0);
    assert!(
        effects.commands.iter().any(|c| matches!(
            c,
            MotorCommand::MoveAbsolute { position, .. } if (position - 80.0).abs() < 1e-9
        )),
        "home completion replays the overtaken target"
    );
    assert_eq!(rec.pos.val, 80.0, "target not synced away");
    assert_eq!(rec.stat.mip, MipFlags::MOVE);
}

#[test]
fn test_home_without_write_syncs_at_completion() {
    // Boundary: val == lval at the home completion — no replay, the
    // normal post-home sync runs.
    let mut rec = MotorRecord::new();
    rec.put_field("HLM", EpicsValue::Double(1000.0)).unwrap();
    rec.put_field("LLM", EpicsValue::Double(-1000.0)).unwrap();
    rec.ctrl.homf = true;
    rec.plan_motion(CommandSource::Homf);
    rec.stat.msta = MstaFlags::MOVING;
    rec.stat.movn = true;

    let effects = complete_stop_at(&mut rec, 5.0);
    assert!(effects.commands.is_empty());
    assert_eq!(rec.pos.val, 5.0, "home completion syncs to readback");
    assert!(rec.stat.dmov);
}

#[test]
fn test_val_written_during_jog_replays_on_sudden_stop() {
    // C 1357-1364 + 1383-1397: the controller stopped the jog on its own
    // (mip collapses to DONE), so the replay gate passes and the VAL
    // written during the jog re-fires.
    let mut rec = MotorRecord::new();
    rec.put_field("HLM", EpicsValue::Double(1000.0)).unwrap();
    rec.put_field("LLM", EpicsValue::Double(-1000.0)).unwrap();
    rec.ctrl.jogf = true;
    rec.plan_motion(CommandSource::Jogf);
    assert_eq!(rec.stat.phase, MotionPhase::Jog);
    rec.stat.msta = MstaFlags::MOVING;
    rec.stat.movn = true;

    rec.put_field("VAL", EpicsValue::Double(80.0)).unwrap();
    let effects = rec.plan_motion(CommandSource::Val);
    assert!(effects.commands.is_empty(), "no dispatch while jogging");

    // Driver reports done without a commanded stop.
    let effects = complete_stop_at(&mut rec, 30.0);
    assert!(
        effects.commands.iter().any(|c| matches!(
            c,
            MotorCommand::MoveAbsolute { position, .. } if (position - 80.0).abs() < 1e-9
        )),
        "sudden jog stop replays the overtaken target"
    );
    assert_eq!(rec.pos.val, 80.0);
}

#[test]
fn test_val_written_during_jog_dropped_on_commanded_stop() {
    // C 1385-1386: MIP_JOG_STOP is excluded from the replay — a release
    // of the jog button drops the mid-jog write via the postProcess sync.
    let mut rec = MotorRecord::new();
    rec.put_field("HLM", EpicsValue::Double(1000.0)).unwrap();
    rec.put_field("LLM", EpicsValue::Double(-1000.0)).unwrap();
    rec.ctrl.jogf = true;
    rec.plan_motion(CommandSource::Jogf);
    rec.stat.msta = MstaFlags::MOVING;
    rec.stat.movn = true;

    rec.put_field("VAL", EpicsValue::Double(80.0)).unwrap();
    rec.plan_motion(CommandSource::Val);

    // Button released: commanded jog stop.
    rec.ctrl.jogf = false;
    rec.plan_motion(CommandSource::Jogf);
    assert_eq!(rec.stat.phase, MotionPhase::JogStopping);

    let effects = complete_stop_at(&mut rec, 30.0);
    assert!(
        !effects
            .commands
            .iter()
            .any(|c| matches!(c, MotorCommand::MoveAbsolute { .. })),
        "commanded jog stop must not replay"
    );
    assert_eq!(rec.pos.val, 30.0, "write dropped by the completion sync");
}

#[test]
fn test_val_written_during_jog_backlash_replays_at_bl2_done() {
    // C: MIP_JOG_BL2 passes the replay gate — a VAL written during the
    // jog backlash correction re-fires when BL2 completes.
    let mut rec = MotorRecord::new();
    rec.put_field("HLM", EpicsValue::Double(1000.0)).unwrap();
    rec.put_field("LLM", EpicsValue::Double(-1000.0)).unwrap();
    rec.retry.bdst = 2.0;
    rec.ctrl.jogf = true;
    rec.plan_motion(CommandSource::Jogf);
    rec.stat.msta = MstaFlags::MOVING;
    rec.stat.movn = true;

    // Commanded stop → jog backlash runs (BL1 then BL2).
    rec.ctrl.jogf = false;
    rec.plan_motion(CommandSource::Jogf);
    let effects = complete_stop_at(&mut rec, 30.0);
    assert_eq!(rec.stat.phase, MotionPhase::JogBacklash);
    assert!(!effects.commands.is_empty(), "BL1 dispatched");
    rec.stat.msta = MstaFlags::MOVING;
    rec.stat.movn = true;

    // VAL written during the backlash: ignored by the dispatcher.
    rec.put_field("VAL", EpicsValue::Double(80.0)).unwrap();
    let effects = rec.plan_motion(CommandSource::Val);
    assert!(effects.commands.is_empty());

    // BL1 done → BL2 dispatched.
    let effects = complete_stop_at(&mut rec, 28.0);
    assert!(!effects.commands.is_empty(), "BL2 dispatched");
    rec.stat.msta = MstaFlags::MOVING;
    rec.stat.movn = true;

    // BL2 done → replay toward the overtaken target.
    let effects = complete_stop_at(&mut rec, 30.0);
    assert!(
        effects.commands.iter().any(|c| matches!(
            c,
            MotorCommand::MoveAbsolute { position, .. } if (position - 80.0).abs() < 1e-9
        )),
        "BL2 completion replays the overtaken target"
    );
}

#[test]
fn test_sub_step_replay_quiesces_without_loop() {
    // A replayed target within one motor step quiesces (commit: too_small
    // lasts update) — the next completion pass must not re-replay.
    let mut rec = MotorRecord::new();
    rec.put_field("HLM", EpicsValue::Double(1000.0)).unwrap();
    rec.put_field("LLM", EpicsValue::Double(-1000.0)).unwrap();
    rec.ctrl.homf = true;
    rec.plan_motion(CommandSource::Homf);
    rec.stat.msta = MstaFlags::MOVING;
    rec.stat.movn = true;

    // Written target lands within one step of the home rest position.
    rec.put_field("VAL", EpicsValue::Double(5.4)).unwrap();
    rec.plan_motion(CommandSource::Val);

    let effects = complete_stop_at(&mut rec, 5.0);
    assert!(
        !effects
            .commands
            .iter()
            .any(|c| matches!(c, MotorCommand::MoveAbsolute { .. })),
        "sub-step replay must quiesce, not dispatch"
    );
    assert_eq!(rec.internal.lval, 5.4, "lasts adopted the written target");
    assert_eq!(rec.stat.mip, MipFlags::empty());

    // Another completion pass: nothing left to replay.
    let effects = complete_stop_at(&mut rec, 5.0);
    assert!(effects.commands.is_empty(), "no replay loop");
}

// --- LVIO re-evaluation during an active jog (C 1462-1483) ---

/// Start a forward jog from rest with HLM=100/LLM=-100, JVEL=1, then
/// report the axis moving.
fn jogging_record() -> MotorRecord {
    let mut rec = MotorRecord::new();
    rec.put_field("HLM", EpicsValue::Double(100.0)).unwrap();
    rec.put_field("LLM", EpicsValue::Double(-100.0)).unwrap();
    // JVEL has no dbd initial (defaults 0.0 = "not configured"); set it
    // as a .db would — the LVIO jog window tests depend on JVEL=1.
    rec.vel.jvel = 1.0;
    rec.ctrl.jogf = true;
    rec.plan_motion(CommandSource::Jogf);
    assert_eq!(rec.stat.phase, MotionPhase::Jog);
    rec.stat.msta = MstaFlags::MOVING;
    rec.stat.movn = true;
    rec
}

/// One in-flight poll: update the readbacks, keep MSTA=MOVING, run the
/// completion pipeline like a DeviceUpdate pass.
fn poll_moving_at(rec: &mut MotorRecord, pos: f64) -> ProcessEffects {
    rec.pos.rbv = pos;
    rec.pos.drbv = pos;
    rec.check_completion()
}

#[test]
fn test_jog_into_soft_limit_window_stops_axis() {
    // C 1466-1468: jogf && rbv > hlm - jvel raises LVIO from the live
    // readback; 1476-1482 stops the axis and releases the buttons.
    let mut rec = jogging_record();

    let effects = poll_moving_at(&mut rec, 50.0);
    assert!(!rec.limits.lvio, "well inside the window: no violation");
    assert!(effects.commands.is_empty());

    let effects = poll_moving_at(&mut rec, 99.5);
    assert!(rec.limits.lvio, "rbv > hlm - jvel raises LVIO");
    assert!(
        effects
            .commands
            .iter()
            .any(|c| matches!(c, MotorCommand::Stop { .. })),
        "rising LVIO stops the axis"
    );
    assert!(!rec.ctrl.jogf, "buttons released (C clear_buttons, 1481)");
    assert_eq!(rec.stat.mip, MipFlags::STOP);

    // Next deceleration poll: MIP_STOP is outside the jog family, the
    // latched LVIO is preserved and no second stop fires.
    let effects = poll_moving_at(&mut rec, 99.8);
    assert!(rec.limits.lvio);
    assert!(effects.commands.is_empty());

    // The jog dispatch armed pp: the stop completion syncs to rest.
    complete_stop_at(&mut rec, 99.9);
    assert!(rec.stat.dmov);
    assert_eq!(rec.pos.val, 99.9, "post-stop sync to readback");
}

#[test]
fn test_jog_away_from_limit_does_not_stop() {
    // jogr near the HIGH limit: the reverse-jog window (rbv < llm + jvel)
    // is not violated, so the jog keeps running.
    let mut rec = MotorRecord::new();
    rec.put_field("HLM", EpicsValue::Double(100.0)).unwrap();
    rec.put_field("LLM", EpicsValue::Double(-100.0)).unwrap();
    rec.pos.rbv = 95.0;
    rec.pos.drbv = 95.0;
    rec.pos.val = 95.0;
    rec.pos.dval = 95.0;
    rec.ctrl.jogr = true;
    rec.plan_motion(CommandSource::Jogr);
    assert_eq!(rec.stat.phase, MotionPhase::Jog);
    rec.stat.msta = MstaFlags::MOVING;
    rec.stat.movn = true;

    let effects = poll_moving_at(&mut rec, 99.5);
    assert!(!rec.limits.lvio);
    assert!(effects.commands.is_empty());
    assert!(rec.ctrl.jogr, "jog keeps running");
}

#[test]
fn test_home_search_disables_lvio_check() {
    // C 1471-1472: MIP_HOME forces lvio = false even when the readback
    // runs outside the soft limits.
    let mut rec = MotorRecord::new();
    rec.put_field("HLM", EpicsValue::Double(100.0)).unwrap();
    rec.put_field("LLM", EpicsValue::Double(-100.0)).unwrap();
    rec.ctrl.homf = true;
    rec.plan_motion(CommandSource::Homf);
    assert_eq!(rec.stat.phase, MotionPhase::Homing);
    rec.stat.msta = MstaFlags::MOVING;
    rec.stat.movn = true;
    rec.limits.lvio = true; // latched from an earlier refusal

    let effects = poll_moving_at(&mut rec, 150.0);
    assert!(!rec.limits.lvio, "home search clears/disables LVIO");
    assert!(effects.commands.is_empty());
    // ctrl.homf is a pulse (cleared at dispatch); MIP_HOMF marks the
    // home in flight.
    assert!(rec.stat.mip.contains(MipFlags::HOMF), "home keeps running");
    assert_eq!(rec.stat.phase, MotionPhase::Homing);
}

#[test]
fn test_jog_lvio_in_set_mode_does_not_stop() {
    // C 1478: the stop fires only when !set && !igset. LVIO itself is
    // still raised and posted.
    let mut rec = jogging_record();
    rec.conv.set = true;

    let effects = poll_moving_at(&mut rec, 99.5);
    assert!(rec.limits.lvio, "violation still recorded");
    assert!(effects.commands.is_empty(), "SET mode suppresses the stop");
    assert!(rec.ctrl.jogf, "buttons untouched");
    assert_eq!(rec.stat.phase, MotionPhase::Jog);
}

// --- Latent tweak collection (C 2167-2181) ---

#[test]
fn test_latent_tweak_folds_into_val_write() {
    // A bare TWF put latched the button but its last_write was overtaken
    // by a VAL write before the record processed. C folds the tweak into
    // the same pass's val processing (2167-2181 precede 2204): one
    // combined move toward written-VAL + TWV.
    let mut rec = MotorRecord::new();
    rec.put_field("HLM", EpicsValue::Double(1000.0)).unwrap();
    rec.put_field("LLM", EpicsValue::Double(-1000.0)).unwrap();
    rec.ctrl.twv = 5.0;
    rec.ctrl.twf = true; // bare put, never dispatched

    rec.put_field("VAL", EpicsValue::Double(50.0)).unwrap();
    let effects = rec.plan_motion(CommandSource::Val);
    assert_eq!(rec.pos.val, 55.0, "tweak folded into the written target");
    assert!(!rec.ctrl.twf, "button consumed by the fold");
    let moves: Vec<f64> = effects
        .commands
        .iter()
        .filter_map(|c| match c {
            MotorCommand::MoveAbsolute { position, .. } => Some(*position),
            _ => None,
        })
        .collect();
    assert_eq!(moves.len(), 1, "one combined move, not two");
    assert!((moves[0] - 55.0).abs() < 1e-9);
}

#[test]
fn test_latent_tweak_twf_priority_when_both_latched() {
    // C 2169: `val += twv * (twf ? 1 : -1)` — TWF wins when both buttons
    // are latched, and BOTH are released (2171-2180).
    let mut rec = MotorRecord::new();
    rec.put_field("HLM", EpicsValue::Double(1000.0)).unwrap();
    rec.put_field("LLM", EpicsValue::Double(-1000.0)).unwrap();
    rec.ctrl.twv = 5.0;
    rec.ctrl.twf = true;
    rec.ctrl.twr = true;

    rec.put_field("VAL", EpicsValue::Double(50.0)).unwrap();
    rec.plan_motion(CommandSource::Val);
    assert_eq!(rec.pos.val, 55.0, "forward fold (TWF priority)");
    assert!(!rec.ctrl.twf);
    assert!(!rec.ctrl.twr, "both buttons released");
}

#[test]
fn test_tweak_under_spmg_pause_collects_and_fires_on_go() {
    // C: the tweak fold (2167) is not gated by stop_or_pause — only the
    // move dispatch is (2237). A tweak pressed under SPMG=Pause lands in
    // VAL/DVAL and Go fires the move, like any write collected while
    // stopped.
    let mut rec = MotorRecord::new();
    rec.put_field("HLM", EpicsValue::Double(1000.0)).unwrap();
    rec.put_field("LLM", EpicsValue::Double(-1000.0)).unwrap();
    rec.pos.val = 10.0;
    rec.pos.dval = 10.0;
    rec.internal.lval = 10.0;
    rec.internal.ldvl = 10.0;
    rec.ctrl.twv = 5.0;
    rec.ctrl.spmg = SpmgMode::Pause;
    rec.internal.lspg = SpmgMode::Pause;

    rec.ctrl.twf = true;
    let effects = rec.plan_motion(CommandSource::Twf);
    assert_eq!(rec.pos.val, 15.0, "fold collected under Pause");
    assert!(!rec.ctrl.twf);
    assert!(
        !effects
            .commands
            .iter()
            .any(|c| matches!(c, MotorCommand::MoveAbsolute { .. })),
        "no dispatch while paused"
    );

    rec.ctrl.spmg = SpmgMode::Go;
    let effects = rec.plan_motion(CommandSource::Spmg);
    assert!(
        effects.commands.iter().any(|c| matches!(
            c,
            MotorCommand::MoveAbsolute { position, .. } if (position - 15.0).abs() < 1e-9
        )),
        "Go fires the collected tweak"
    );
}

// --- Latent SYNC (C 2540-2544) ---

#[test]
fn test_sync_while_idle_applies_immediately() {
    // C: sync != 0 && mip == MIP_DONE at the chain end of an idle pass
    // — syncTargetPosition runs and the latch consumes.
    let mut rec = MotorRecord::new();
    rec.put_field("HLM", EpicsValue::Double(1000.0)).unwrap();
    rec.put_field("LLM", EpicsValue::Double(-1000.0)).unwrap();
    rec.pos.val = 50.0;
    rec.pos.dval = 50.0;
    rec.pos.drbv = 20.0;
    rec.pos.rbv = 20.0;

    rec.put_field("SYNC", EpicsValue::Short(1)).unwrap();
    assert_eq!(rec.get_field("SYNC"), Some(EpicsValue::Short(1)));
    rec.plan_motion(CommandSource::Sync);
    assert_eq!(rec.pos.val, 20.0, "target synced to readback");
    assert!(!rec.internal.sync, "latch consumed");
    assert_eq!(rec.get_field("SYNC"), Some(EpicsValue::Short(0)));
}

#[test]
fn test_sync_during_move_defers_to_completion() {
    // C: during the move mip != MIP_DONE — the latch survives the pass
    // (no mid-move target yank) and consumes at the completion pass
    // (do_work runs on dmov, 1485).
    let mut rec = MotorRecord::new();
    rec.put_field("HLM", EpicsValue::Double(1000.0)).unwrap();
    rec.put_field("LLM", EpicsValue::Double(-1000.0)).unwrap();
    rec.put_field("VAL", EpicsValue::Double(50.0)).unwrap();
    rec.plan_motion(CommandSource::Val);
    rec.stat.msta = MstaFlags::MOVING;
    rec.stat.movn = true;

    rec.put_field("SYNC", EpicsValue::Short(1)).unwrap();
    rec.plan_motion(CommandSource::Sync);
    assert_eq!(rec.pos.val, 50.0, "no mid-move yank");
    assert!(rec.internal.sync, "latch survives the busy pass");

    complete_stop_at(&mut rec, 49.9995);
    assert!(rec.stat.dmov);
    assert!(!rec.internal.sync, "latch consumed at completion");
    assert_eq!(rec.pos.val, 49.9995, "completion applied the sync");
}

#[test]
fn test_latent_sync_overtaken_by_position_write() {
    // A bare SYNC put whose last_write was overtaken by a VAL write: the
    // move dispatches (C move block fires, sync else-if skipped) and the
    // latch consumes at that move's completion.
    let mut rec = MotorRecord::new();
    rec.put_field("HLM", EpicsValue::Double(1000.0)).unwrap();
    rec.put_field("LLM", EpicsValue::Double(-1000.0)).unwrap();
    rec.put_field("SYNC", EpicsValue::Short(1)).unwrap();
    rec.put_field("VAL", EpicsValue::Double(50.0)).unwrap();

    let effects = rec.plan_motion(CommandSource::Val);
    assert!(
        effects
            .commands
            .iter()
            .any(|c| matches!(c, MotorCommand::MoveAbsolute { .. })),
        "the position write still dispatches"
    );
    assert!(rec.internal.sync, "latch not lost to the overtake");
    rec.stat.msta = MstaFlags::MOVING;
    rec.stat.movn = true;

    complete_stop_at(&mut rec, 49.9995);
    assert!(!rec.internal.sync, "latch consumed at completion");
    assert_eq!(rec.pos.val, 49.9995);
}

#[test]
fn test_sync_under_pause_defers_until_go() {
    // C: SPMG Stop/Pause returns at 2237 before the sync else-if — the
    // latch survives the pause and applies on the Go pass.
    let mut rec = MotorRecord::new();
    rec.put_field("HLM", EpicsValue::Double(1000.0)).unwrap();
    rec.put_field("LLM", EpicsValue::Double(-1000.0)).unwrap();
    rec.pos.val = 50.0;
    rec.pos.dval = 50.0;
    rec.internal.lval = 50.0;
    rec.internal.ldvl = 50.0;
    rec.pos.drbv = 20.0;
    rec.pos.rbv = 20.0;
    rec.ctrl.spmg = SpmgMode::Pause;
    rec.internal.lspg = SpmgMode::Pause;

    rec.put_field("SYNC", EpicsValue::Short(1)).unwrap();
    rec.plan_motion(CommandSource::Sync);
    assert_eq!(rec.pos.val, 50.0, "no apply under Pause");
    assert!(rec.internal.sync);

    rec.ctrl.spmg = SpmgMode::Go;
    let effects = rec.plan_motion(CommandSource::Spmg);
    assert!(
        !effects
            .commands
            .iter()
            .any(|c| matches!(c, MotorCommand::MoveAbsolute { .. })),
        "nothing to resume (dval == ldvl, dmov)"
    );
    assert_eq!(rec.pos.val, 20.0, "Go pass applies the latched sync");
    assert!(!rec.internal.sync);
}

// --- Sudden jog stop: no backlash (C 1357-1364 + postProcess 908) ---

#[test]
fn test_sudden_jog_stop_skips_backlash() {
    // C: a controller self-stop collapses mip to MIP_DONE (1357-1364);
    // postProcess's backlash branch requires MIP_JOG_STOP or MIP_MOVE
    // (908) — so no correction fires, the record syncs and finishes.
    let mut rec = MotorRecord::new();
    rec.put_field("HLM", EpicsValue::Double(1000.0)).unwrap();
    rec.put_field("LLM", EpicsValue::Double(-1000.0)).unwrap();
    rec.retry.bdst = 2.0;
    rec.ctrl.jogf = true;
    rec.plan_motion(CommandSource::Jogf);
    assert_eq!(rec.stat.phase, MotionPhase::Jog);
    rec.stat.msta = MstaFlags::MOVING;
    rec.stat.movn = true;

    // Driver reports done with no commanded stop.
    let effects = complete_stop_at(&mut rec, 30.0);
    assert!(
        !effects
            .commands
            .iter()
            .any(|c| matches!(c, MotorCommand::MoveAbsolute { .. })),
        "no backlash correction on a controller self-stop"
    );
    assert!(rec.stat.dmov);
    assert_eq!(rec.pos.val, 30.0, "synced at the rest position");
    assert_eq!(rec.stat.phase, MotionPhase::Idle);
}

#[test]
fn test_commanded_jog_stop_still_corrects_backlash() {
    // Boundary: the commanded release (JogStopping = C MIP_JOG_STOP)
    // keeps the postProcess backlash correction (932-947).
    let mut rec = MotorRecord::new();
    rec.put_field("HLM", EpicsValue::Double(1000.0)).unwrap();
    rec.put_field("LLM", EpicsValue::Double(-1000.0)).unwrap();
    rec.retry.bdst = 2.0;
    rec.ctrl.jogf = true;
    rec.plan_motion(CommandSource::Jogf);
    rec.stat.msta = MstaFlags::MOVING;
    rec.stat.movn = true;

    rec.ctrl.jogf = false;
    rec.plan_motion(CommandSource::Jogf);
    assert_eq!(rec.stat.phase, MotionPhase::JogStopping);

    let effects = complete_stop_at(&mut rec, 30.0);
    assert_eq!(rec.stat.phase, MotionPhase::JogBacklash);
    assert!(
        effects
            .commands
            .iter()
            .any(|c| matches!(c, MotorCommand::MoveAbsolute { .. })),
        "commanded jog stop dispatches the backlash correction"
    );
}

// --- LVIO re-evaluation on the write pass (C 1462-1483 + pp(TRUE) limits) ---

#[test]
fn test_hlm_put_during_jog_stops_on_write_pass() {
    // HLM/LLM/DHLM/DLLM are pp(TRUE) in C: lowering a limit onto a
    // running jog processes the record, the enter_do_work re-evaluation
    // trips and the same pass stops the axis — no poll round-trip.
    let mut rec = jogging_record(); // HLM=100, LLM=-100, JVEL=1, jogging fwd
    rec.pos.rbv = 50.0;
    rec.pos.drbv = 50.0;

    // Lower HLM to 50.5: rbv > hlm - jvel immediately.
    rec.put_field("HLM", EpicsValue::Double(50.5)).unwrap();
    // The put sets no last_write — the framework's process pass lands in
    // the no-event arm.
    let _ = rec.do_process();
    assert!(rec.limits.lvio, "write pass raised LVIO");
    assert_eq!(rec.stat.mip, MipFlags::STOP, "write pass stopped the axis");
    assert!(!rec.ctrl.jogf, "buttons released");
}

#[test]
fn test_lvio_recompute_runs_on_command_pass_during_jog() {
    // C runs the same re-evaluation before do_work on every
    // put-processing pass: a VAL write landing while the jog has run
    // into the limit window stops the axis instead of dispatching.
    let mut rec = jogging_record();
    rec.pos.rbv = 99.5; // already inside the violation window
    rec.pos.drbv = 99.5;

    rec.put_field("VAL", EpicsValue::Double(20.0)).unwrap();
    let effects = rec.plan_motion(CommandSource::Val);
    assert!(rec.limits.lvio);
    assert_eq!(rec.stat.mip, MipFlags::STOP);
    assert!(
        effects
            .commands
            .iter()
            .any(|c| matches!(c, MotorCommand::Stop { .. })),
        "the pass stops instead of dispatching the write"
    );
    assert!(
        !effects
            .commands
            .iter()
            .any(|c| matches!(c, MotorCommand::MoveAbsolute { .. })),
        "no move dispatched on the stop pass"
    );
}

#[test]
fn test_hlm_put_while_idle_does_not_stop() {
    // Boundary: the same put while idle preserves the latch (mip empty
    // -> C else-branch) and sends nothing.
    let mut rec = MotorRecord::new();
    rec.put_field("HLM", EpicsValue::Double(100.0)).unwrap();
    rec.put_field("LLM", EpicsValue::Double(-100.0)).unwrap();
    rec.pos.rbv = 50.0;
    rec.pos.drbv = 50.0;
    rec.put_field("HLM", EpicsValue::Double(50.5)).unwrap();
    let effects = rec.do_process();
    assert!(effects.commands.is_empty());
    assert_eq!(rec.stat.mip, MipFlags::empty());
}

// === MIP_JOG_REQ lifecycle and queued/resumed MIP readback ===
// C special() 3042-3054 (arm/clear), do_work 2082 (park across
// stop_or_pause), 2106/2111 (wholesale consume, queued = JOGF|STOP),
// 2023/2028 (queued home = HOMF|STOP), postProcess 866 (resume keeps
// the direction bit), top block 1901-1922 (wholesale STOP / Go re-arm).

/// Settle an idle record into SPMG=Pause: the Pause transition parks
/// mip = MIP_STOP (C 1901-1905); the stop completion collapses it back
/// to DONE via maybeRetry's close-enough branch.
fn paused_idle_record() -> MotorRecord {
    let mut rec = MotorRecord::new();
    rec.put_field("HLM", EpicsValue::Double(100.0)).unwrap();
    rec.put_field("LLM", EpicsValue::Double(-100.0)).unwrap();
    rec.ctrl.spmg = SpmgMode::Pause;
    rec.plan_motion(CommandSource::Spmg);
    assert_eq!(rec.stat.mip, MipFlags::STOP);
    let _ = complete_stop_at(&mut rec, 0.0);
    assert_eq!(rec.stat.mip, MipFlags::empty(), "pause settles to DONE");
    rec
}

#[test]
fn test_jogf_put_while_paused_parks_jog_req() {
    // C special() 3045-3046: JOGF=1 on an idle record (mip == MIP_DONE,
    // !hls) arms MIP_JOG_REQ; the dispatch gate (2082) refuses while
    // stop_or_pause, so the request parks in the MIP readback.
    let mut rec = paused_idle_record();
    rec.put_field("JOGF", EpicsValue::Short(1)).unwrap();
    assert_eq!(rec.stat.mip, MipFlags::JOG_REQ, "armed at the put");
    let effects = rec.plan_motion(CommandSource::Jogf);
    assert!(effects.commands.is_empty(), "no dispatch while paused");
    assert_eq!(rec.stat.mip, MipFlags::JOG_REQ, "request stays parked");

    // Go re-arms from the button and dispatches; the wholesale
    // `mip = MIP_JOGF` consumes the parked request (C 1914-1918 + 2106).
    rec.ctrl.spmg = SpmgMode::Go;
    let effects = rec.plan_motion(CommandSource::Spmg);
    assert!(
        effects.commands.iter().any(|c| matches!(
            c,
            MotorCommand::MoveVelocity {
                direction: true,
                ..
            }
        )),
        "Go dispatches the parked jog"
    );
    assert_eq!(rec.stat.mip, MipFlags::JOGF, "JOG_REQ consumed at dispatch");
}

#[test]
fn test_jogf_release_clears_parked_jog_req() {
    // C special() 3043-3044: jogf == 0 clears a parked MIP_JOG_REQ.
    let mut rec = paused_idle_record();
    rec.put_field("JOGF", EpicsValue::Short(1)).unwrap();
    assert_eq!(rec.stat.mip, MipFlags::JOG_REQ);
    rec.put_field("JOGF", EpicsValue::Short(0)).unwrap();
    assert_eq!(rec.stat.mip, MipFlags::empty());

    // Go with nothing parked dispatches no jog.
    rec.ctrl.spmg = SpmgMode::Go;
    let effects = rec.plan_motion(CommandSource::Spmg);
    assert!(
        !effects
            .commands
            .iter()
            .any(|c| matches!(c, MotorCommand::MoveVelocity { .. }))
    );
}

#[test]
fn test_jogf_put_at_limit_switch_does_not_arm_jog_req() {
    // C special() 3045: `mip == MIP_DONE && !pmr->hls` — at the limit
    // switch the press latches the button but arms nothing.
    let mut rec = paused_idle_record();
    rec.limits.hls = true;
    rec.put_field("JOGF", EpicsValue::Short(1)).unwrap();
    assert!(rec.ctrl.jogf);
    assert_eq!(rec.stat.mip, MipFlags::empty());
}

#[test]
fn test_stop_kills_parked_jog_req_but_keeps_button() {
    // C's stop branch early-returns only for mip DONE/STOP/RETRY
    // (1874-1888); a parked MIP_JOG_REQ falls through to the wholesale
    // `mip = MIP_STOP` (1901-1905). clear_buttons runs only in the movn
    // branch (1893), so the button itself survives.
    let mut rec = paused_idle_record();
    rec.put_field("JOGF", EpicsValue::Short(1)).unwrap();
    assert_eq!(rec.stat.mip, MipFlags::JOG_REQ);
    rec.put_field("STOP", EpicsValue::Short(1)).unwrap();
    rec.plan_motion(CommandSource::Stop);
    assert_eq!(rec.stat.mip, MipFlags::STOP, "parked request killed");
    assert!(rec.ctrl.jogf, "button survives the stop");
}

#[test]
fn test_jogf_release_while_idle_is_noop() {
    // C 2148 gate: stop-jogging requires MIP_JOGF/JOGR in mip. A release
    // on an idle record must not fabricate a deceleration.
    let mut rec = MotorRecord::new();
    rec.put_field("HLM", EpicsValue::Double(100.0)).unwrap();
    rec.put_field("LLM", EpicsValue::Double(-100.0)).unwrap();
    rec.put_field("JOGF", EpicsValue::Short(0)).unwrap();
    let effects = rec.plan_motion(CommandSource::Jogf);
    assert!(effects.commands.is_empty(), "no spurious stop command");
    assert_eq!(rec.stat.phase, MotionPhase::Idle);
    assert_eq!(rec.stat.mip, MipFlags::empty());
}

#[test]
fn test_queued_jog_resumes_with_jogf_mip_and_live_lvio() {
    let mut rec = MotorRecord::new();
    rec.put_field("HLM", EpicsValue::Double(100.0)).unwrap();
    rec.put_field("LLM", EpicsValue::Double(-100.0)).unwrap();
    // JVEL has no dbd initial (defaults 0.0); the LVIO window below
    // (rbv > hlm - jvel) needs a configured JVEL=1.
    rec.vel.jvel = 1.0;
    rec.put_field("VAL", EpicsValue::Double(50.0)).unwrap();
    rec.plan_motion(CommandSource::Val);
    rec.stat.msta = MstaFlags::MOVING;
    rec.stat.movn = true;

    // JOGF mid-move: stop first, queue the jog — readback JOGF|STOP
    // (C 2106 wholesale + 2111 `mip |= MIP_STOP`).
    rec.put_field("JOGF", EpicsValue::Short(1)).unwrap();
    let effects = rec.plan_motion(CommandSource::Jogf);
    assert!(
        effects
            .commands
            .iter()
            .any(|c| matches!(c, MotorCommand::Stop { .. }))
    );
    assert_eq!(rec.stat.mip, MipFlags::JOGF | MipFlags::STOP);

    // Stop completes: the resume drops STOP and keeps the direction bit
    // (C postProcess 866 `mip &= ~MIP_STOP`).
    let effects = complete_stop_at(&mut rec, 30.0);
    assert!(
        effects
            .commands
            .iter()
            .any(|c| matches!(c, MotorCommand::MoveVelocity { .. }))
    );
    assert_eq!(rec.stat.mip, MipFlags::JOGF, "resumed jog visible in MIP");
    assert_eq!(rec.stat.phase, MotionPhase::Jog);

    // Regression: the resumed jog must satisfy the live-RBV LVIO
    // recompute (C 1466-1468), which keys on the MIP jog bits — with the
    // old empty MIP the jog would coast through the soft-limit window.
    rec.stat.msta = MstaFlags::MOVING;
    rec.stat.movn = true;
    let effects = poll_moving_at(&mut rec, 99.5);
    assert!(rec.limits.lvio, "rbv > hlm - jvel raises LVIO");
    assert!(
        effects
            .commands
            .iter()
            .any(|c| matches!(c, MotorCommand::Stop { .. })),
        "rising LVIO stops the resumed jog"
    );
    assert!(!rec.ctrl.jogf, "buttons released (C 1481 clear_buttons)");
}

#[test]
fn test_queued_home_resumes_with_homf_mip() {
    let mut rec = MotorRecord::new();
    rec.put_field("HLM", EpicsValue::Double(1000.0)).unwrap();
    rec.put_field("LLM", EpicsValue::Double(-1000.0)).unwrap();
    rec.put_field("VAL", EpicsValue::Double(50.0)).unwrap();
    rec.plan_motion(CommandSource::Val);
    rec.stat.msta = MstaFlags::MOVING;
    rec.stat.movn = true;

    rec.ctrl.homf = true;
    rec.plan_motion(CommandSource::Homf);
    assert_eq!(
        rec.stat.mip,
        MipFlags::HOMF | MipFlags::STOP,
        "queued home readback (C 2023/2028)"
    );

    let effects = complete_stop_at(&mut rec, 30.0);
    assert!(
        effects
            .commands
            .iter()
            .any(|c| matches!(c, MotorCommand::Home { forward: true, .. }))
    );
    assert_eq!(rec.stat.mip, MipFlags::HOMF, "C 866 keeps the HOME bit");
    assert_eq!(rec.stat.phase, MotionPhase::Homing);
}

#[test]
fn test_jogf_release_cancels_queued_jog() {
    // C 2148-2155 on a queued jog (mip = JOGF|STOP): the release pass
    // drops JOGF from MIP, so the request dies with the in-flight stop.
    let mut rec = MotorRecord::new();
    rec.put_field("HLM", EpicsValue::Double(100.0)).unwrap();
    rec.put_field("LLM", EpicsValue::Double(-100.0)).unwrap();
    rec.put_field("VAL", EpicsValue::Double(50.0)).unwrap();
    rec.plan_motion(CommandSource::Val);
    rec.stat.msta = MstaFlags::MOVING;
    rec.stat.movn = true;
    rec.put_field("JOGF", EpicsValue::Short(1)).unwrap();
    rec.plan_motion(CommandSource::Jogf);
    assert_eq!(rec.stat.mip, MipFlags::JOGF | MipFlags::STOP);

    rec.put_field("JOGF", EpicsValue::Short(0)).unwrap();
    rec.plan_motion(CommandSource::Jogf);
    assert!(rec.internal.queued_motion.is_none(), "queued jog cancelled");
    assert_eq!(rec.stat.mip, MipFlags::STOP | MipFlags::JOG_STOP);

    let effects = complete_stop_at(&mut rec, 30.0);
    assert!(
        !effects
            .commands
            .iter()
            .any(|c| matches!(c, MotorCommand::MoveVelocity { .. })),
        "cancelled request must not resurrect at the stop completion"
    );
    assert!(rec.stat.dmov);
}

#[test]
fn test_bare_release_kills_queued_jog_at_resume() {
    // A release that lands only in the button state (overtaken in
    // last_write, never processed as Jogf) must still kill the queued
    // jog at the resume point — C kills it on the release pass itself
    // (2148-2155), so a resurrected jog with no held button has no C
    // counterpart.
    let mut rec = MotorRecord::new();
    rec.put_field("HLM", EpicsValue::Double(100.0)).unwrap();
    rec.put_field("LLM", EpicsValue::Double(-100.0)).unwrap();
    rec.put_field("VAL", EpicsValue::Double(50.0)).unwrap();
    rec.plan_motion(CommandSource::Val);
    rec.stat.msta = MstaFlags::MOVING;
    rec.stat.movn = true;
    rec.put_field("JOGF", EpicsValue::Short(1)).unwrap();
    rec.plan_motion(CommandSource::Jogf);
    assert_eq!(rec.stat.mip, MipFlags::JOGF | MipFlags::STOP);

    rec.ctrl.jogf = false; // bare release, no process pass
    let effects = complete_stop_at(&mut rec, 30.0);
    assert!(
        !effects
            .commands
            .iter()
            .any(|c| matches!(c, MotorCommand::MoveVelocity { .. })),
        "released queued jog must not resurrect"
    );
    assert!(rec.stat.dmov, "plain pp stop finalizes");
    assert_eq!(rec.stat.mip, MipFlags::empty());
}

#[test]
fn test_go_with_jog_button_at_limit_switch_collapses_stop() {
    // C 1914-1922: at the limit switch the latched button does not
    // re-arm the jog; the bare MIP_STOP collapses to DONE instead of
    // leaving the record stuck in STOP.
    let mut rec = MotorRecord::new();
    rec.put_field("HLM", EpicsValue::Double(100.0)).unwrap();
    rec.put_field("LLM", EpicsValue::Double(-100.0)).unwrap();
    rec.ctrl.spmg = SpmgMode::Pause;
    rec.plan_motion(CommandSource::Spmg);
    assert_eq!(rec.stat.mip, MipFlags::STOP);

    rec.ctrl.jogf = true; // latched during the pause
    rec.limits.hls = true;
    rec.ctrl.spmg = SpmgMode::Go;
    let effects = rec.plan_motion(CommandSource::Spmg);
    assert!(
        !effects
            .commands
            .iter()
            .any(|c| matches!(c, MotorCommand::MoveVelocity { .. })),
        "C 1914: jogf && !hls required to re-arm"
    );
    assert_eq!(rec.stat.mip, MipFlags::empty(), "STOP collapses to DONE");
}

#[test]
fn test_jog_direction_folds_dir_into_dial_command() {
    // C 2119: jogv = (jvel * dir) / mres — the commanded velocity sign
    // carries DIR, so a JOGF (user-forward) on a DIR=Neg axis must be
    // dial-negative. CDIR stays raw: jogf, inverted for DIR=Neg and
    // MRES<0 (C 2126-2137).
    let mut rec = MotorRecord::new();
    rec.put_field("HLM", EpicsValue::Double(100.0)).unwrap();
    rec.put_field("LLM", EpicsValue::Double(-100.0)).unwrap();
    rec.put_field("DIR", EpicsValue::Short(1)).unwrap(); // Neg
    rec.ctrl.jogf = true;
    let effects = rec.plan_motion(CommandSource::Jogf);
    let dial_dir = effects.commands.iter().find_map(|c| match c {
        MotorCommand::MoveVelocity { direction, .. } => Some(*direction),
        _ => None,
    });
    assert_eq!(
        dial_dir,
        Some(false),
        "JOGF with DIR=Neg commands dial-negative (C 2119 jvel*dir)"
    );
    assert!(!rec.stat.cdir, "CDIR raw-frame: jogf ^ DIR=Neg ^ MRES>0");
}

#[test]
fn test_queued_jog_replay_folds_dir_and_updates_cdir() {
    // The queued-jog replay re-enters the C jog section (2118-2143) on
    // re-fire, re-deriving the commanded direction and CDIR — the
    // replay must match a fresh dispatch on a DIR=Neg axis.
    let mut rec = MotorRecord::new();
    rec.put_field("HLM", EpicsValue::Double(100.0)).unwrap();
    rec.put_field("LLM", EpicsValue::Double(-100.0)).unwrap();
    rec.put_field("DIR", EpicsValue::Short(1)).unwrap(); // Neg
    // Start a move, then press JOGF mid-move: queues Jog + STOP.
    rec.put_field("VAL", EpicsValue::Double(50.0)).unwrap();
    rec.plan_motion(CommandSource::Val);
    rec.stat.msta = MstaFlags::MOVING;
    rec.stat.movn = true;
    rec.ctrl.jogf = true;
    rec.plan_motion(CommandSource::Jogf);
    assert_eq!(rec.stat.mip, MipFlags::JOGF | MipFlags::STOP);

    // Stop completes → queued jog replays.
    rec.stat.msta = MstaFlags::DONE;
    rec.stat.movn = false;
    let effects = rec.check_completion();
    let dial_dir = effects.commands.iter().find_map(|c| match c {
        MotorCommand::MoveVelocity { direction, .. } => Some(*direction),
        _ => None,
    });
    assert_eq!(
        dial_dir,
        Some(false),
        "queued JOGF replay on DIR=Neg commands dial-negative"
    );
    assert!(
        !rec.stat.cdir,
        "replay refreshes CDIR like C's re-entered jog section"
    );
}

#[test]
fn test_go_during_pause_deceleration_resumes_move() {
    // C 1862-1925: Pause mid-move stops the axis (wholesale MIP_STOP);
    // a Go written BEFORE the stop completes collapses MIP_STOP to DONE
    // on that same pass (C runs the top block ungated) and falls
    // through to the move block, which re-fires the unfinished move
    // (!dmov, C 2241/2455) — the new MOVE supersedes the decelerating
    // stop in the controller. The Go edge must not be dropped.
    let mut rec = MotorRecord::new();
    rec.put_field("HLM", EpicsValue::Double(100.0)).unwrap();
    rec.put_field("LLM", EpicsValue::Double(-100.0)).unwrap();
    rec.put_field("VAL", EpicsValue::Double(50.0)).unwrap();
    rec.plan_motion(CommandSource::Val);
    rec.stat.msta = MstaFlags::MOVING;
    rec.stat.movn = true;

    rec.ctrl.spmg = SpmgMode::Pause;
    rec.plan_motion(CommandSource::Spmg);
    assert_eq!(rec.stat.mip, MipFlags::STOP);

    // Go arrives while still decelerating (no completion poll yet).
    rec.ctrl.spmg = SpmgMode::Go;
    let effects = rec.plan_motion(CommandSource::Spmg);
    assert!(
        effects.commands.iter().any(|c| matches!(
            c,
            MotorCommand::MoveAbsolute { position, .. } if (*position - 50.0).abs() < 1e-9
        )),
        "Go during the pause deceleration must re-fire the move"
    );
    assert!(rec.stat.mip.contains(MipFlags::MOVE));
}

#[test]
fn test_go_with_queued_home_defers_to_replay_not_move_dispatch() {
    // C 2455 dispatch gate: mip = HOMF|STOP (queued home) is neither
    // MIP_DONE nor MIP_RETRY, so a Go pass must NOT dispatch a move
    // over it; the queued home replays at stop completion (856-890).
    let mut rec = MotorRecord::new();
    rec.put_field("HLM", EpicsValue::Double(100.0)).unwrap();
    rec.put_field("LLM", EpicsValue::Double(-100.0)).unwrap();
    rec.ctrl.spmg = SpmgMode::Move; // so the later Go write is an edge
    rec.plan_motion(CommandSource::Spmg);
    rec.put_field("VAL", EpicsValue::Double(50.0)).unwrap();
    rec.plan_motion(CommandSource::Val);
    rec.stat.msta = MstaFlags::MOVING;
    rec.stat.movn = true;
    rec.ctrl.homf = true;
    rec.plan_motion(CommandSource::Homf);
    assert_eq!(rec.stat.mip, MipFlags::HOMF | MipFlags::STOP);

    rec.ctrl.spmg = SpmgMode::Go;
    let effects = rec.plan_motion(CommandSource::Spmg);
    assert!(
        !effects
            .commands
            .iter()
            .any(|c| matches!(c, MotorCommand::MoveAbsolute { .. })),
        "queued home keeps its replay path; Go must not dispatch a move"
    );
    assert_eq!(rec.stat.mip, MipFlags::HOMF | MipFlags::STOP);

    rec.stat.msta = MstaFlags::DONE;
    rec.stat.movn = false;
    let effects = rec.check_completion();
    assert!(
        effects
            .commands
            .iter()
            .any(|c| matches!(c, MotorCommand::Home { .. })),
        "queued home replays at stop completion"
    );
}

#[test]
fn test_spmg_move_during_deceleration_abandons_queued_jog_and_resumes() {
    // C 1927-1933: the Move arm assigns mip = MIP_DONE WHOLESALE with
    // rcnt = 0 — a queued jog loses its MIP direction bits (never
    // replayed) and the move block re-fires the unfinished target.
    let mut rec = MotorRecord::new();
    rec.put_field("HLM", EpicsValue::Double(100.0)).unwrap();
    rec.put_field("LLM", EpicsValue::Double(-100.0)).unwrap();
    rec.put_field("VAL", EpicsValue::Double(50.0)).unwrap();
    rec.plan_motion(CommandSource::Val);
    rec.stat.msta = MstaFlags::MOVING;
    rec.stat.movn = true;
    rec.ctrl.jogf = true;
    rec.plan_motion(CommandSource::Jogf);
    assert_eq!(rec.stat.mip, MipFlags::JOGF | MipFlags::STOP);

    rec.ctrl.spmg = SpmgMode::Move;
    let effects = rec.plan_motion(CommandSource::Spmg);
    assert!(
        effects
            .commands
            .iter()
            .any(|c| matches!(c, MotorCommand::MoveAbsolute { .. })),
        "Move resumes the unfinished target"
    );
    assert!(rec.stat.mip.contains(MipFlags::MOVE));

    // Completion: the abandoned jog must not replay.
    rec.stat.msta = MstaFlags::DONE;
    rec.stat.movn = false;
    rec.pos.rbv = 50.0;
    rec.pos.drbv = 50.0;
    let effects = rec.check_completion();
    assert!(
        !effects
            .commands
            .iter()
            .any(|c| matches!(c, MotorCommand::MoveVelocity { .. })),
        "queued jog was abandoned wholesale by the Move resume (C 1927-1933)"
    );
}

#[test]
fn test_mlst_alst_writable_by_framework_deadband_owner() {
    // C monitor() 3485-3501 anchors the MDEL/ADEL deadbands at the
    // last POSTED readback by writing MLST/ALST. The framework's
    // deadband owner performs that write through put_field
    // (put_coerced); the arms must accept it even though the fields
    // stay SPC_NOMOD toward CA. Pre-fix the write was FieldNotFound
    // (silently swallowed) and the anchor read 0.0 forever.
    let mut rec = MotorRecord::new();
    rec.put_field("MLST", EpicsValue::Double(3.5)).unwrap();
    rec.put_field("ALST", EpicsValue::Double(4.5)).unwrap();
    assert_eq!(rec.get_field("MLST"), Some(EpicsValue::Double(3.5)));
    assert_eq!(rec.get_field("ALST"), Some(EpicsValue::Double(4.5)));
}

// C alarm_sub (motorRecord.cc:3367-3406) ported as the Record::check_alarms
// hook. One test per C arm/boundary.
mod alarm_sub {
    use super::*;
    use epics_base_rs::server::recgbl::alarm_status;
    use epics_base_rs::server::record::{AlarmSeverity, CommonFields};

    fn common() -> CommonFields {
        CommonFields {
            udf: false,
            ..Default::default()
        }
    }

    // C 3372-3376: UDF raises UDF_ALARM/INVALID and short-circuits the
    // limit checks — even with a limit switch active.
    #[test]
    fn udf_short_circuits_all_other_alarms() {
        let mut rec = MotorRecord::new();
        rec.limits.hlsv = 2; // MAJOR
        rec.limits.hls = true;
        let mut c = common();
        c.udf = true;
        rec.check_alarms(&mut c);
        assert_eq!(c.nsta, alarm_status::UDF_ALARM);
        assert_eq!(c.nsev, AlarmSeverity::Invalid);
    }

    // C 3379-3383: HLS at HLSV severity.
    #[test]
    fn hls_raises_high_alarm_at_hlsv() {
        let mut rec = MotorRecord::new();
        rec.limits.hlsv = 2; // MAJOR
        rec.limits.hls = true;
        let mut c = common();
        rec.check_alarms(&mut c);
        assert_eq!(c.nsta, alarm_status::HIGH_ALARM);
        assert_eq!(c.nsev, AlarmSeverity::Major);
    }

    // C 3379: dval STRICTLY above DHLM trips the soft-limit arm; exactly
    // at the limit does not.
    #[test]
    fn dval_above_dhlm_raises_high_alarm_but_at_limit_does_not() {
        let mut rec = MotorRecord::new();
        rec.limits.hlsv = 1; // MINOR
        rec.limits.dhlm = 10.0;
        rec.limits.dllm = -10.0;
        rec.pos.dval = 10.0;
        let mut c = common();
        rec.check_alarms(&mut c);
        assert_eq!(
            c.nsta,
            alarm_status::NO_ALARM,
            "dval == dhlm must not alarm"
        );

        rec.pos.dval = 10.5;
        let mut c = common();
        rec.check_alarms(&mut c);
        assert_eq!(c.nsta, alarm_status::HIGH_ALARM);
        assert_eq!(c.nsev, AlarmSeverity::Minor);
    }

    // C 3384-3388: the LOW arm gates on (and raises at) HLSV too — the
    // motor record has no low-side severity field.
    #[test]
    fn lls_raises_low_alarm_at_hlsv() {
        let mut rec = MotorRecord::new();
        rec.limits.hlsv = 3; // INVALID
        rec.limits.lls = true;
        let mut c = common();
        rec.check_alarms(&mut c);
        assert_eq!(c.nsta, alarm_status::LOW_ALARM);
        assert_eq!(c.nsev, AlarmSeverity::Invalid);
    }

    #[test]
    fn dval_below_dllm_raises_low_alarm() {
        let mut rec = MotorRecord::new();
        rec.limits.hlsv = 2;
        rec.limits.dhlm = 10.0;
        rec.limits.dllm = -10.0;
        rec.pos.dval = -10.5;
        let mut c = common();
        rec.check_alarms(&mut c);
        assert_eq!(c.nsta, alarm_status::LOW_ALARM);
        assert_eq!(c.nsev, AlarmSeverity::Major);
    }

    // C: `if (pmr->hlsv && ...)` — HLSV=NO_ALARM disables both limit arms.
    #[test]
    fn hlsv_no_alarm_gates_limit_alarms_off() {
        let mut rec = MotorRecord::new();
        rec.limits.hlsv = 0;
        rec.limits.hls = true;
        rec.limits.lls = true;
        let mut c = common();
        rec.check_alarms(&mut c);
        assert_eq!(c.nsta, alarm_status::NO_ALARM);
        assert_eq!(c.nsev, AlarmSeverity::NoAlarm);
    }

    // C 3392-3398: CNTRL_COMM_ERR raises COMM/INVALID and CLEARS the MSTA
    // bit (one-shot until the driver re-reports).
    #[test]
    fn comm_err_raises_comm_invalid_and_clears_msta_bit() {
        let mut rec = MotorRecord::new();
        rec.stat.msta = MstaFlags::COMM_ERR | MstaFlags::DONE;
        let mut c = common();
        rec.check_alarms(&mut c);
        assert_eq!(c.nsta, alarm_status::COMM_ALARM);
        assert_eq!(c.nsev, AlarmSeverity::Invalid);
        assert!(
            !rec.stat.msta.contains(MstaFlags::COMM_ERR),
            "C clears msta.Bits.CNTRL_COMM_ERR after raising the alarm"
        );
        assert!(rec.stat.msta.contains(MstaFlags::DONE), "other bits kept");

        // Second pass without a driver re-report: no comm alarm.
        let mut c2 = common();
        rec.check_alarms(&mut c2);
        assert_eq!(c2.nsta, alarm_status::NO_ALARM);
    }

    // C 3400-3403: EA_SLIP_STALL or RA_PROBLEM → STATE/MAJOR.
    #[test]
    fn slip_stall_or_problem_raises_state_major() {
        for bit in [MstaFlags::SLIP_STALL, MstaFlags::PROBLEM] {
            let mut rec = MotorRecord::new();
            rec.stat.msta = bit;
            let mut c = common();
            rec.check_alarms(&mut c);
            assert_eq!(c.nsta, alarm_status::STATE_ALARM, "{bit:?}");
            assert_eq!(c.nsev, AlarmSeverity::Major, "{bit:?}");
        }
    }

    // C falls through from the comm check to the slip/stall check;
    // recGblSetSevr keeps the higher severity, so COMM/INVALID wins.
    #[test]
    fn comm_err_and_problem_accumulate_comm_invalid_wins() {
        let mut rec = MotorRecord::new();
        rec.stat.msta = MstaFlags::COMM_ERR | MstaFlags::PROBLEM;
        let mut c = common();
        rec.check_alarms(&mut c);
        assert_eq!(c.nsta, alarm_status::COMM_ALARM);
        assert_eq!(c.nsev, AlarmSeverity::Invalid);
    }

    // C 3379-3388: an active limit switch outranks a comm/state alarm —
    // the limit arms return early.
    #[test]
    fn limit_switch_arm_returns_before_msta_checks() {
        let mut rec = MotorRecord::new();
        rec.limits.hlsv = 2;
        rec.limits.hls = true;
        rec.stat.msta = MstaFlags::COMM_ERR;
        let mut c = common();
        rec.check_alarms(&mut c);
        assert_eq!(c.nsta, alarm_status::HIGH_ALARM);
        assert!(
            rec.stat.msta.contains(MstaFlags::COMM_ERR),
            "early return must leave the comm bit for the next pass"
        );
    }
}

// C process_exit (motorRecord.cc:1509-1510, restored by c970afbf after
// the 0ef39053 transition-only experiment): FLNK fires on every process
// pass that exits with DMOV true. Two passes the old per-path
// suppression wrongly silenced — both leave DMOV at 1, so C fires FLNK.
mod flnk_dmov_gate {
    use super::*;

    // A VAL write swallowed by the SPDB window (steps >= 1, inside
    // SPDB) leaves DMOV at 1 the whole pass — C's too_small branch
    // returns OK without touching dmov (2329-2348) and process_exit
    // fires FLNK.
    #[test]
    fn spdb_suppressed_write_with_dmov_high_fires_flnk() {
        let mut rec = MotorRecord::new();
        rec.conv.mres = 1.0;
        rec.retry.spdb = 5.0;
        assert!(rec.stat.dmov);
        rec.put_field("VAL", EpicsValue::Double(2.0)).unwrap();
        rec.set_event(MotorEvent::UserWrite(CommandSource::Val));
        let _ = rec.process();
        assert!(rec.stat.dmov, "SPDB-suppressed write must not drop DMOV");
        assert!(
            rec.should_fire_forward_link(),
            "C fires FLNK on the too-small pass (dmov stayed 1)"
        );
    }

    // A jog refused at a soft limit (C 2087-2105) clears the button and
    // returns OK with DMOV untouched — FLNK fires on that pass.
    #[test]
    fn refused_jog_at_soft_limit_fires_flnk() {
        let mut rec = MotorRecord::new();
        rec.conv.mres = 1.0;
        rec.vel.jvel = 1.0;
        rec.limits.hlm = 10.0;
        rec.limits.llm = -10.0;
        rec.limits.dhlm = 10.0;
        rec.limits.dllm = -10.0;
        rec.pos.val = 10.0; // at HLM: val >= hlm - refusal
        rec.put_field("JOGF", EpicsValue::Short(1)).unwrap();
        rec.set_event(MotorEvent::UserWrite(CommandSource::Jogf));
        let _ = rec.process();
        assert!(!rec.ctrl.jogf, "refused jog must release the button");
        assert!(rec.stat.dmov, "refused jog never starts motion");
        assert!(
            rec.should_fire_forward_link(),
            "C fires FLNK on the refusal pass (dmov stayed 1)"
        );
    }
}

// C set_user_highlimit / set_user_lowlimit / set_dial_highlimit /
// set_dial_lowlimit (motorRecord.cc:4076-4328): every limit write moves
// exactly ONE field in the other frame — the pair is never re-ordered.
mod one_sided_limit_writes {
    use super::*;

    // Writing HLM below the existing LLM must NOT swap the dial pair:
    // DHLM alone moves, the inverted pair persists, and LVIO latches.
    #[test]
    fn hlm_below_llm_preserves_inverted_dial_pair_and_latches_lvio() {
        let mut rec = MotorRecord::new();
        rec.conv.mres = 1.0;
        rec.put_field("HLM", EpicsValue::Double(10.0)).unwrap();
        rec.put_field("LLM", EpicsValue::Double(-10.0)).unwrap();
        assert!(!rec.limits.lvio || rec.pos.dval > 10.0);

        rec.put_field("HLM", EpicsValue::Double(-20.0)).unwrap();
        assert_eq!(rec.limits.dhlm, -20.0, "only DHLM moves");
        assert_eq!(rec.limits.dllm, -10.0, "DLLM must stay as written");
        assert!(rec.limits.dllm > rec.limits.dhlm, "inverted pair kept");
        assert!(rec.limits.lvio, "inverted pair must latch LVIO");
    }

    // DIR=Neg: an HLM write lands in DLLM only (C 4090-4092), an LLM
    // write in DHLM only (C 4169-4171).
    #[test]
    fn dir_neg_hlm_write_updates_dllm_only() {
        let mut rec = MotorRecord::new();
        rec.conv.dir = MotorDir::Neg;
        rec.pos.off = 5.0;
        rec.limits.dhlm = 77.0;
        rec.put_field("HLM", EpicsValue::Double(100.0)).unwrap();
        assert_eq!(rec.limits.dllm, -95.0); // -(100 - 5)
        assert_eq!(rec.limits.dhlm, 77.0, "DHLM untouched");
    }

    // DIR=Neg: a DHLM write lands in LLM only (C 4261-4264), a DLLM
    // write in HLM only (C 4312-4315).
    #[test]
    fn dir_neg_dhlm_write_updates_llm_only() {
        let mut rec = MotorRecord::new();
        rec.conv.dir = MotorDir::Neg;
        rec.conv.mres = 1.0;
        rec.pos.off = 5.0;
        rec.limits.hlm = 33.0;
        rec.put_field("DHLM", EpicsValue::Double(50.0)).unwrap();
        assert_eq!(rec.limits.llm, -45.0); // -(50) + 5
        assert_eq!(rec.limits.hlm, 33.0, "HLM untouched");
    }
}

// =============================================================================
// Runtime velocity clamps — C special() range_check and the VBAS/VMAX
// coupling tail (motorRecord.cc:2627-2732, 3056-3082, 3107-3148)
// =============================================================================

mod velocity_clamps {
    use super::*;

    fn inited() -> MotorRecord {
        let mut rec = MotorRecord::new();
        rec.conv.urev = 1.0;
        rec.init_record(1).unwrap();
        rec
    }

    // C 2690: a VELO above VMAX is clamped down; S follows.
    #[test]
    fn velo_clamped_into_vbas_vmax_window() {
        let mut rec = inited();
        rec.put_field("VBAS", EpicsValue::Double(0.5)).unwrap();
        rec.put_field("VMAX", EpicsValue::Double(4.0)).unwrap();
        rec.put_field("VELO", EpicsValue::Double(9.0)).unwrap();
        assert_eq!(rec.vel.velo, 4.0);
        assert_eq!(rec.vel.s, 4.0);
        rec.put_field("VELO", EpicsValue::Double(0.1)).unwrap();
        assert_eq!(rec.vel.velo, 0.5);
    }

    // C 4364-4372: max == 0 means "no ceiling" — only the VBAS floor applies.
    #[test]
    fn vmax_zero_means_no_ceiling() {
        let mut rec = inited();
        rec.put_field("VBAS", EpicsValue::Double(0.5)).unwrap();
        rec.put_field("VELO", EpicsValue::Double(900.0)).unwrap();
        assert_eq!(rec.vel.velo, 900.0);
    }

    // C 2629-2633 / 2660-2664: VBAS and VMAX are forced non-negative.
    #[test]
    fn negative_vbas_and_vmax_forced_to_zero() {
        let mut rec = inited();
        rec.put_field("VBAS", EpicsValue::Double(-1.0)).unwrap();
        assert_eq!(rec.vel.vbas, 0.0);
        rec.put_field("VMAX", EpicsValue::Double(-2.0)).unwrap();
        assert_eq!(rec.vel.vmax, 0.0);
    }

    // C 3110-3117: VMAX dropped below VBAS drags VBAS (and SBAS) down,
    // then every dependent velocity re-clamps into the new window.
    #[test]
    fn vmax_below_vbas_drags_vbas_down_and_reclamps() {
        let mut rec = inited();
        rec.put_field("VBAS", EpicsValue::Double(2.0)).unwrap();
        rec.put_field("VMAX", EpicsValue::Double(10.0)).unwrap();
        rec.put_field("VELO", EpicsValue::Double(8.0)).unwrap();
        rec.put_field("JVEL", EpicsValue::Double(8.0)).unwrap();
        rec.put_field("VMAX", EpicsValue::Double(1.0)).unwrap();
        assert_eq!(rec.vel.vbas, 1.0, "VBAS dragged down to VMAX");
        assert_eq!(rec.vel.sbas, 1.0, "SBAS follows VBAS");
        assert_eq!(rec.vel.velo, 1.0, "VELO re-clamped");
        assert_eq!(rec.vel.jvel, 1.0, "JVEL re-clamped");
    }

    // C 3121-3127: VBAS raised above VMAX drags VMAX (and SMAX) up.
    #[test]
    fn vbas_above_vmax_drags_vmax_up() {
        let mut rec = inited();
        rec.put_field("VMAX", EpicsValue::Double(3.0)).unwrap();
        rec.put_field("VBAS", EpicsValue::Double(5.0)).unwrap();
        assert_eq!(rec.vel.vmax, 5.0, "VMAX dragged up to VBAS");
        assert_eq!(rec.vel.smax, 5.0, "SMAX follows VMAX");
        assert_eq!(rec.vel.velo, 5.0, "VELO re-clamped up to the new floor");
    }

    // C 3128-3148 velcheckA: BVEL/JVEL/HVEL re-clamp on a VBAS write, and
    // SBAK follows the clamped BVEL.
    #[test]
    fn vbas_write_reclamps_bvel_jvel_hvel() {
        let mut rec = inited();
        rec.put_field("VMAX", EpicsValue::Double(10.0)).unwrap();
        rec.put_field("BVEL", EpicsValue::Double(0.2)).unwrap();
        rec.put_field("JVEL", EpicsValue::Double(0.2)).unwrap();
        rec.put_field("HVEL", EpicsValue::Double(0.2)).unwrap();
        rec.put_field("VBAS", EpicsValue::Double(1.0)).unwrap();
        assert_eq!(rec.vel.bvel, 1.0);
        assert_eq!(rec.vel.sbak, 1.0);
        assert_eq!(rec.vel.jvel, 1.0);
        assert_eq!(rec.vel.hvel, 1.0);
    }

    // C 2702 / 2725: the rev-unit pair members clamp in rev units
    // ([SBAS, SMAX]) and their EGU partner follows.
    #[test]
    fn s_and_sbak_clamp_in_rev_units() {
        let mut rec = inited();
        rec.conv.urev = 2.0;
        rec.put_field("SMAX", EpicsValue::Double(3.0)).unwrap();
        rec.put_field("S", EpicsValue::Double(5.0)).unwrap();
        assert_eq!(rec.vel.s, 3.0);
        assert_eq!(rec.vel.velo, 6.0, "VELO = S * |UREV|");
        rec.put_field("SBAK", EpicsValue::Double(7.0)).unwrap();
        assert_eq!(rec.vel.sbak, 3.0);
        assert_eq!(rec.vel.bvel, 6.0, "BVEL = SBAK * |UREV|");
    }

    // The clamps are C special() semantics — runtime only. During
    // dbLoadRecords (pre-init) every field() must land raw.
    #[test]
    fn clamps_inert_before_init() {
        let mut rec = MotorRecord::new();
        rec.put_field("VBAS", EpicsValue::Double(-1.0)).unwrap();
        assert_eq!(rec.vel.vbas, -1.0, "raw during load");
        rec.put_field("VELO", EpicsValue::Double(900.0)).unwrap();
        assert_eq!(rec.vel.velo, 900.0, "raw during load");
    }
}

// =============================================================================
// Init-time JVEL/JAR/HVEL derivation — C check_speed_and_resolution
// (motorRecord.cc:4054-4067)
// =============================================================================

mod init_jog_home_velocity_defaults {
    use super::*;

    // C 4055-4056: an unconfigured JVEL (no dbd initial → 0) takes VELO.
    #[test]
    fn unconfigured_jvel_defaults_to_velo() {
        let mut rec = MotorRecord::new();
        rec.put_field("VELO", EpicsValue::Double(3.0)).unwrap();
        rec.init_record(1).unwrap();
        assert_eq!(rec.vel.jvel, 3.0);
    }

    // C 4057-4058: a configured JVEL is clamped into [VBAS, VMAX].
    #[test]
    fn configured_jvel_clamped_at_init() {
        let mut rec = MotorRecord::new();
        rec.put_field("VMAX", EpicsValue::Double(2.0)).unwrap();
        rec.put_field("JVEL", EpicsValue::Double(9.0)).unwrap();
        rec.init_record(1).unwrap();
        assert_eq!(rec.vel.jvel, 2.0);
    }

    // C 4060-4061: an unconfigured JAR takes VELO/ACCL.
    #[test]
    fn unconfigured_jar_defaults_to_velo_over_accl() {
        let mut rec = MotorRecord::new();
        rec.put_field("VELO", EpicsValue::Double(4.0)).unwrap();
        rec.put_field("ACCL", EpicsValue::Double(0.5)).unwrap();
        rec.init_record(1).unwrap();
        assert_eq!(rec.vel.jar, 8.0);
    }

    // C 4064-4065: an unconfigured HVEL takes VBAS.
    #[test]
    fn unconfigured_hvel_defaults_to_vbas() {
        let mut rec = MotorRecord::new();
        rec.put_field("VBAS", EpicsValue::Double(0.7)).unwrap();
        rec.init_record(1).unwrap();
        assert_eq!(rec.vel.hvel, 0.7);
    }

    // C 4066-4067: a configured HVEL is clamped into [VBAS, VMAX].
    #[test]
    fn configured_hvel_clamped_at_init() {
        let mut rec = MotorRecord::new();
        rec.put_field("VBAS", EpicsValue::Double(0.5)).unwrap();
        rec.put_field("HVEL", EpicsValue::Double(0.1)).unwrap();
        rec.init_record(1).unwrap();
        assert_eq!(rec.vel.hvel, 0.5);
    }
}

// =============================================================================
// Live JVEL retune + runtime JAR fallback — C special() motorRecordJVEL /
// motorRecordJAR (motorRecord.cc:3056-3078)
// =============================================================================

mod jvel_live_retune {
    use super::*;

    fn jogging_after_init(forward: bool) -> MotorRecord {
        let mut rec = MotorRecord::new();
        rec.put_field("HLM", EpicsValue::Double(100.0)).unwrap();
        rec.put_field("LLM", EpicsValue::Double(-100.0)).unwrap();
        rec.put_field("VELO", EpicsValue::Double(1.0)).unwrap();
        rec.init_record(1).unwrap(); // JVEL <- VELO = 1.0
        if forward {
            rec.ctrl.jogf = true;
            rec.plan_motion(CommandSource::Jogf);
        } else {
            rec.ctrl.jogr = true;
            rec.plan_motion(CommandSource::Jogr);
        }
        assert_eq!(rec.stat.phase, MotionPhase::Jog);
        rec.stat.msta = MstaFlags::MOVING;
        rec.stat.movn = true;
        rec
    }

    // C 3059-3072: JVEL written mid-jog re-sends the jog command with
    // the new velocity, same direction.
    #[test]
    fn jvel_write_during_active_jog_reemits_move_velocity() {
        let mut rec = jogging_after_init(true);
        rec.put_field("JVEL", EpicsValue::Double(2.5)).unwrap();
        let effects = rec.do_process();
        assert!(
            effects.commands.iter().any(|c| matches!(
                c,
                MotorCommand::MoveVelocity {
                    direction: true,
                    velocity,
                    ..
                } if *velocity == 2.5
            )),
            "retune re-emits the jog at the new JVEL: {:?}",
            effects.commands
        );
    }

    // C 3064-3065: a reverse jog keeps its direction through the retune.
    #[test]
    fn jvel_retune_keeps_reverse_direction() {
        let mut rec = jogging_after_init(false);
        rec.put_field("JVEL", EpicsValue::Double(2.5)).unwrap();
        let effects = rec.do_process();
        assert!(
            effects.commands.iter().any(|c| matches!(
                c,
                MotorCommand::MoveVelocity {
                    direction: false,
                    ..
                }
            )),
            "reverse jog retunes in the reverse direction: {:?}",
            effects.commands
        );
    }

    // C 3059: the retune is gated on an active MIP jog bit — a JVEL
    // write while idle emits nothing.
    #[test]
    fn jvel_write_when_idle_emits_nothing() {
        let mut rec = MotorRecord::new();
        rec.init_record(1).unwrap();
        rec.put_field("JVEL", EpicsValue::Double(2.5)).unwrap();
        let effects = rec.do_process();
        assert!(
            !effects
                .commands
                .iter()
                .any(|c| matches!(c, MotorCommand::MoveVelocity { .. })),
            "no jog active, no retune: {:?}",
            effects.commands
        );
    }

    // C 3074-3078: a non-positive JAR put is replaced by JVEL / 0.1 at
    // runtime.
    #[test]
    fn nonpositive_jar_put_takes_jvel_over_tenth() {
        let mut rec = MotorRecord::new();
        rec.init_record(1).unwrap();
        rec.put_field("JVEL", EpicsValue::Double(2.0)).unwrap();
        rec.put_field("JAR", EpicsValue::Double(-1.0)).unwrap();
        assert_eq!(rec.vel.jar, 20.0);
        rec.put_field("JAR", EpicsValue::Double(0.0)).unwrap();
        assert_eq!(rec.vel.jar, 20.0);
    }

    // During load the JAR fallback must stay inert: field() lands raw and
    // init derives the unconfigured value from VELO/ACCL.
    #[test]
    fn jar_zero_raw_during_load_then_init_derives() {
        let mut rec = MotorRecord::new();
        rec.put_field("VELO", EpicsValue::Double(4.0)).unwrap();
        rec.put_field("ACCL", EpicsValue::Double(0.5)).unwrap();
        rec.put_field("JAR", EpicsValue::Double(0.0)).unwrap();
        assert_eq!(rec.vel.jar, 0.0, "raw during load");
        rec.init_record(1).unwrap();
        assert_eq!(rec.vel.jar, 8.0, "init derives VELO/ACCL");
    }
}

// =============================================================================
// DLY watchdog orphan guard — C callbackFunc (motorRecord.cc:460-480)
// =============================================================================

mod dly_orphan_guard {
    use super::*;

    // A DelayExpired that fires after the wait was rescinded (a new move
    // replaced MIP and dropped DELAY_REQ) must do nothing — C callbackFunc
    // ignores the orphaned watchdog.
    #[test]
    fn orphaned_delay_expiry_is_ignored() {
        let mut rec = MotorRecord::new();
        rec.put_field("HLM", EpicsValue::Double(100.0)).unwrap();
        rec.put_field("LLM", EpicsValue::Double(-100.0)).unwrap();
        // Mid-move state with no DELAY_REQ armed (the wait was rescinded).
        rec.stat.mip = MipFlags::MOVE;
        rec.stat.dmov = false;
        rec.set_event(MotorEvent::DelayExpired);
        let effects = rec.do_process();
        assert!(
            !rec.stat.mip.contains(MipFlags::DELAY_ACK),
            "orphaned expiry must not set DELAY_ACK"
        );
        assert!(!effects.status_refresh, "no status refresh for an orphan");
        assert_eq!(rec.stat.mip, MipFlags::MOVE, "MIP untouched");
    }

    // A live DELAY_REQ converts to DELAY_ACK with a status refresh
    // (C 470-478 REQ->ACK + scanOnce; the refresh is the GET_INFO step).
    #[test]
    fn armed_delay_expiry_acks_and_refreshes() {
        let mut rec = MotorRecord::new();
        rec.stat.mip = MipFlags::MOVE | MipFlags::DELAY_REQ;
        rec.stat.dmov = false;
        rec.stat.phase = MotionPhase::DelayWait;
        rec.set_event(MotorEvent::DelayExpired);
        let effects = rec.do_process();
        assert!(rec.stat.mip.contains(MipFlags::DELAY_ACK));
        assert!(!rec.stat.mip.contains(MipFlags::DELAY_REQ));
        assert!(effects.status_refresh);
    }
}

// =============================================================================
// Retry decision deferred past DLY — C process() 1410-1456: maybeRetry runs
// only after the delay watchdog fires and the fresh status lands
// =============================================================================

mod retry_after_dly_settle {
    use super::*;

    // A completion with position error and DLY > 0 must arm the delay
    // (no retry command on the completion pass); the retry dispatches
    // only after DELAY_ACK.
    #[test]
    fn retry_waits_for_dly_settle() {
        let mut rec = MotorRecord::new();
        rec.conv.mres = 0.1;
        rec.retry.rtry = 3;
        rec.retry.rdbd = 0.5;
        rec.timing.dly = 0.2;
        rec.put_field("VAL", EpicsValue::Double(10.0)).unwrap();
        rec.do_process();
        assert_eq!(rec.stat.phase, MotionPhase::MainMove);

        // Driver stops 2.0 short of the target: completion pass.
        rec.stat.msta = MstaFlags::DONE;
        rec.stat.movn = false;
        rec.pos.drbv = 8.0;
        rec.pos.rbv = 8.0;
        let effects = rec.check_completion();
        assert!(
            !effects
                .commands
                .iter()
                .any(|c| matches!(c, MotorCommand::MoveAbsolute { .. })),
            "no retry before the DLY settle: {:?}",
            effects.commands
        );
        assert_eq!(rec.stat.phase, MotionPhase::DelayWait);
        assert!(rec.stat.mip.contains(MipFlags::DELAY_REQ));
        assert!(effects.schedule_delay.is_some(), "delay armed");
        assert_eq!(rec.retry.rcnt, 0, "no retry counted yet");

        // Watchdog fires, refreshed status lands: NOW the retry dispatches.
        rec.set_event(MotorEvent::DelayExpired);
        let effects = rec.do_process();
        assert!(effects.status_refresh, "C GET_INFO before maybeRetry");
        let effects = rec.check_completion();
        assert!(
            effects
                .commands
                .iter()
                .any(|c| matches!(c, MotorCommand::MoveAbsolute { .. })),
            "retry dispatches after the settle: {:?}",
            effects.commands
        );
        assert_eq!(rec.retry.rcnt, 1);
        assert!(rec.stat.mip.contains(MipFlags::RETRY));
    }

    // DLY == 0: the evaluation runs on the completion pass itself.
    #[test]
    fn dly_zero_evaluates_immediately() {
        let mut rec = MotorRecord::new();
        rec.conv.mres = 0.1;
        rec.retry.rtry = 3;
        rec.retry.rdbd = 0.5;
        rec.put_field("VAL", EpicsValue::Double(10.0)).unwrap();
        rec.do_process();
        rec.stat.msta = MstaFlags::DONE;
        rec.stat.movn = false;
        rec.pos.drbv = 8.0;
        rec.pos.rbv = 8.0;
        let effects = rec.check_completion();
        assert!(
            effects
                .commands
                .iter()
                .any(|c| matches!(c, MotorCommand::MoveAbsolute { .. })),
            "immediate retry with DLY=0: {:?}",
            effects.commands
        );
        assert_eq!(rec.retry.rcnt, 1);
    }
}
