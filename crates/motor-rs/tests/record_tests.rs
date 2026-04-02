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
    assert_eq!(rec.vel.accl, 0.5);
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
        high_limit: false,
        low_limit: false,
        home: false,
        powered: true,
        problem: false,
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

    // Same direction, farther -> ExtendMove
    assert_eq!(rec.handle_retarget(15.0), RetargetAction::ExtendMove);

    // Opposite direction -> StopAndReplan
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
fn test_cnen_emits_set_closed_loop() {
    let mut rec = MotorRecord::new();
    rec.ctrl.cnen = true;
    let effects = rec.plan_motion(CommandSource::Cnen);
    assert_eq!(effects.commands.len(), 1);
    assert!(matches!(
        effects.commands[0],
        MotorCommand::SetClosedLoop { enable: true }
    ));
}

#[test]
fn test_cnen_false_emits_disable() {
    let mut rec = MotorRecord::new();
    rec.ctrl.cnen = false;
    let effects = rec.plan_motion(CommandSource::Cnen);
    assert_eq!(effects.commands.len(), 1);
    assert!(matches!(
        effects.commands[0],
        MotorCommand::SetClosedLoop { enable: false }
    ));
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
