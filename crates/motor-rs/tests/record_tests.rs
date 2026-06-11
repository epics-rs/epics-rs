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
    // VAL synced to RBV
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
fn test_spmg_stop_finalizes() {
    let mut rec = MotorRecord::new();
    rec.stat.phase = MotionPhase::MainMove;
    rec.stat.mip = MipFlags::MOVE;
    rec.stat.dmov = false;
    rec.pos.rbv = 25.0;
    rec.pos.drbv = 25.0;
    rec.pos.rrbv = 2500;

    rec.ctrl.spmg = SpmgMode::Stop;
    let effects = rec.plan_motion(CommandSource::Spmg);
    assert!(rec.stat.dmov); // finalized
    assert_eq!(rec.stat.phase, MotionPhase::Idle);
    assert_eq!(rec.pos.val, 25.0); // synced
    assert!(matches!(effects.commands[0], MotorCommand::Stop { .. }));
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
    // After the DLY timer fires, DelayWait finalizes to Idle/DMOV=1.
    let mut rec = MotorRecord::new();
    rec.stat.phase = MotionPhase::DelayWait;
    rec.stat.dmov = false;
    rec.stat.msta = MstaFlags::DONE;
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
    // HLM put first; user-side normalization swaps dial but record-side user
    // pair stays as written, so detect_inverted_limits flags LVIO.
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
fn test_sync_pv_get_returns_zero() {
    let mut rec = MotorRecord::new();
    rec.put_field("SYNC", EpicsValue::Short(1)).unwrap();
    // SYNC is write-only trigger; readback always 0.
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
