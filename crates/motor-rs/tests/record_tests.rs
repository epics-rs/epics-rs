use epics_base_rs::server::record::FieldDeclaration;
use epics_base_rs::server::record::Record;
use epics_base_rs::types::EpicsValue;
use motor_rs::MotorRecord;
use motor_rs::flags::*;

/// Anchor a mailbox-wired record with a first idle status pulse (Startup
/// pass). C's init_record precedes any dbProcess pass, so tests that drive
/// put/scan passes must run them against an anchored record — an unanchored
/// mailbox-wired record parks its signals until the anchor
/// (do_process_inner's anchor-first gate).
fn anchor(rec: &mut MotorRecord, state: &motor_rs::device_state::SharedDeviceState) {
    use asyn_rs::interfaces::motor::MotorStatus;
    use motor_rs::device_state::StampedStatus;
    state.lock().unwrap().latest_status = Some(StampedStatus {
        seq: 1,
        status: MotorStatus {
            done: true,
            ..MotorStatus::default()
        },
    });
    rec.process().unwrap();
}

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
    // C dbd: VELO has no initial() (0.0, like JVEL/HVEL); ACCL carries
    // initial("0.2").
    assert_eq!(rec.vel.velo, 0.0);
    assert_eq!(rec.vel.accl, 0.2);
    assert_eq!(rec.retry.rtry, 10);
    // C dbd LVIO has no initial() and init_record clears it before the
    // initial-readback check (motorRecord.cc:734-743).
    assert!(!rec.limits.lvio);
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
fn test_limit_strike_completes_move_and_clears_buttons() {
    // C: a struck limit in the commanded direction zeroes MOVN
    // (3741-3743) and releases the latched buttons (3744-3747) even
    // while the driver still reports done=false/moving=true; the
    // process callback then takes the stopped branch (1301/1345)
    // instead of waiting for RA_DONE.
    let mut rec = MotorRecord::new();
    rec.conv.mres = 1.0;
    rec.stat.msta = MstaFlags::DONE;
    rec.vel.jvel = 2.0;
    rec.put_field("JOGF", EpicsValue::Short(1)).unwrap();
    rec.plan_motion(CommandSource::Jogf);
    assert_eq!(rec.stat.phase, MotionPhase::Jog);
    assert!(rec.stat.cdir, "forward jog commands positive direction");

    let struck = asyn_rs::interfaces::motor::MotorStatus {
        position: 9.0,
        high_limit: true,
        done: false,
        moving: true,
        ..Default::default()
    };
    rec.process_motor_info(&struck);
    assert!(!rec.stat.movn, "ls_active zeroes MOVN");
    assert!(!rec.ctrl.jogf, "C 3744-3747: buttons released");

    let _ = rec.check_completion();
    assert!(rec.stat.dmov, "the jog completes despite done=false");
    assert_eq!(rec.stat.phase, MotionPhase::Idle);
}

#[test]
fn test_driver_problem_completes_and_clears_buttons() {
    // Same C gate, RA_PROBLEM leg: a driver fault mid-home stops the
    // record's bookkeeping and drops the latched button.
    let mut rec = MotorRecord::new();
    rec.stat.msta = MstaFlags::DONE;
    rec.put_field("HOMF", EpicsValue::Short(1)).unwrap();
    rec.plan_motion(CommandSource::Homf);
    assert_eq!(rec.stat.phase, MotionPhase::Homing);

    let fault = asyn_rs::interfaces::motor::MotorStatus {
        problem: true,
        done: false,
        moving: true,
        ..Default::default()
    };
    rec.process_motor_info(&fault);
    assert!(!rec.stat.movn, "RA_PROBLEM zeroes MOVN");
    assert!(!rec.ctrl.homf, "C 3744-3747: button released");

    let _ = rec.check_completion();
    assert!(rec.stat.dmov, "completion proceeds despite done=false");
}

#[test]
fn test_athm_tracks_home_switch_every_poll() {
    // C 3755-3762: ATHM is the home-switch state on every poll —
    // RA_HOME normally, EA_HOME when UEIP=Yes — and drops when the
    // axis leaves the switch.
    let mut rec = MotorRecord::new();
    rec.conv.mres = 1.0;
    let on_switch = asyn_rs::interfaces::motor::MotorStatus {
        home: true,
        has_encoder: true,
        ..Default::default()
    };
    rec.process_motor_info(&on_switch);
    assert!(rec.stat.athm, "RA_HOME drives ATHM under UEIP=No");

    let off_switch = asyn_rs::interfaces::motor::MotorStatus::default();
    rec.process_motor_info(&off_switch);
    assert!(!rec.stat.athm, "ATHM drops when the switch releases");

    // UEIP=Yes reads the encoder home signal instead.
    rec.conv.ueip = true;
    rec.process_motor_info(&on_switch);
    assert!(!rec.stat.athm, "RA_HOME is ignored under UEIP=Yes");
    let encoder_home = asyn_rs::interfaces::motor::MotorStatus {
        encoder_home: true,
        has_encoder: true,
        ..Default::default()
    };
    rec.process_motor_info(&encoder_home);
    assert!(rec.stat.athm, "EA_HOME drives ATHM under UEIP=Yes");
}

#[test]
fn test_process_motor_info_marks_diff_rdif_for_force_post() {
    // C process_motor_info (motorRecord.cc:3764-3767) MARKs M_DIFF/M_RDIF
    // on every CALLBACK_DATA pass; monitor() (3522-3531) then posts both
    // with DBE_VAL_LOG regardless of change. The Rust record records the
    // mark so the framework re-posts DIFF/RDIF this cycle even when the
    // following error is constant. A record that has not run
    // process_motor_info this pass must name no force-posted field.
    let mut rec = MotorRecord::new();
    rec.conv.mres = 1.0;
    assert!(
        rec.force_posted_fields().is_empty(),
        "a fresh record (no CALLBACK_DATA pass) force-posts nothing"
    );

    let status = asyn_rs::interfaces::motor::MotorStatus::default();
    rec.process_motor_info(&status);
    assert_eq!(
        rec.force_posted_fields(),
        &["DIFF", "RDIF"],
        "a CALLBACK_DATA pass marks DIFF/RDIF for unconditional re-post"
    );
}

#[test]
fn test_sync_positions() {
    let mut rec = MotorRecord::new();
    rec.conv.mres = 0.01;
    rec.pos.drbv = 5.0;
    rec.pos.rbv = 5.0;
    rec.pos.rrbv = 500;
    rec.sync_positions();
    assert_eq!(rec.pos.dval, 5.0);
    assert_eq!(rec.pos.val, 5.0);
    // C 843: rval = NINT(dval / mres)
    assert_eq!(rec.pos.rval, 500);
    assert_eq!(rec.pos.diff, 0.0);
}

#[test]
fn test_sync_positions_rval_is_motor_raw_not_rrbv() {
    // C 712/843/4455: the synced RVAL is NINT(dval/mres) — under
    // UEIP=Yes RRBV holds encoder counts (ERES scale), which must not
    // land in the raw MOTOR command field.
    let mut rec = MotorRecord::new();
    rec.conv.mres = 0.001;
    rec.conv.eres = 0.002;
    rec.conv.ueip = true;
    let status = asyn_rs::interfaces::motor::MotorStatus {
        position: 10.0,
        encoder_position: 10.0,
        done: true,
        has_encoder: true,
        ..Default::default()
    };
    rec.process_motor_info(&status);
    assert_eq!(rec.pos.rrbv, 5000, "encoder counts");

    rec.sync_positions();
    assert_eq!(rec.pos.dval, 10.0);
    assert_eq!(rec.pos.rval, 10000, "raw motor steps = dval/mres");
    assert_eq!(rec.internal.lrvl, 10000);
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
    // C does not clear the button at dispatch: HOMF reads back 1 for
    // the whole home and clears at completion (postProcess 893-906).
    assert!(rec.ctrl.homf);
    assert!(matches!(
        effects.commands[0],
        MotorCommand::Home { forward: true, .. }
    ));
}

#[test]
fn test_set_mode_offset_redefinition_retranslates_user_limits() {
    // C set_userlimits at every offset redefinition: SET VAL collection
    // (motorRecord.cc:2220) and the load_pos Variable leg (3800) —
    // HLM/LLM re-derive from DHLM/DLLM through the new offset.
    let mut rec = MotorRecord::new();
    rec.conv.mres = 1.0;
    rec.stat.msta = MstaFlags::DONE;
    rec.put_field("DHLM", EpicsValue::Double(10.0)).unwrap();
    rec.put_field("DLLM", EpicsValue::Double(-10.0)).unwrap();
    rec.pos.val = 5.0;
    rec.pos.dval = 5.0;
    rec.put_field("SET", EpicsValue::Short(1)).unwrap();

    // DVAL redefinition (FOFF=Variable default): off = 5 - 0 = 5.
    rec.put_field("DVAL", EpicsValue::Double(0.0)).unwrap();
    assert_eq!(rec.pos.off, 5.0);
    assert_eq!(rec.limits.hlm, 15.0, "HLM follows the new offset");
    assert_eq!(rec.limits.llm, -5.0, "LLM follows the new offset");

    // VAL redefinition: off = 7 - 0 = 7.
    rec.put_field("VAL", EpicsValue::Double(7.0)).unwrap();
    assert_eq!(rec.pos.off, 7.0);
    assert_eq!(rec.limits.hlm, 17.0);
    assert_eq!(rec.limits.llm, -3.0);
}

#[test]
fn test_set_foff_variable_val_completes_without_command() {
    // C 2206-2227: SET + FOFF=Variable VAL acts directly — offset and
    // RBV retranslate, mip = MIP_DONE, dmov = TRUE, and the pass
    // returns before the move block: no LOAD_POS, no motion.
    let mut rec = MotorRecord::new();
    rec.conv.mres = 1.0;
    rec.stat.msta = MstaFlags::DONE;
    rec.pos.val = 5.0;
    rec.pos.dval = 5.0;
    rec.pos.drbv = 5.0;
    rec.stat.dmov = false; // a stale low DMOV completes on the spot
    rec.put_field("SET", EpicsValue::Short(1)).unwrap();

    rec.put_field("VAL", EpicsValue::Double(100.0)).unwrap();
    assert_eq!(rec.pos.off, 95.0);
    assert_eq!(rec.pos.dval, 5.0, "DVAL untouched");
    assert_eq!(rec.pos.rbv, 100.0, "RBV retranslated through new OFF");
    assert!(rec.stat.dmov, "C 2225: dmov = TRUE");
    assert!(rec.stat.mip.is_empty(), "C 2223: mip = MIP_DONE");
}

#[test]
fn test_set_dval_load_pos_pulses_dmov_and_refreshes_status() {
    // C load_pos (motorRecord.cc:3771-3817): SET-mode DVAL re-anchors
    // the controller — LOAD_POS then GET_INFO, mip = MIP_LOAD_P, DMOV
    // pulses low; the status callback completes it (1404-1409).
    let mut rec = MotorRecord::new();
    rec.conv.mres = 1.0;
    rec.stat.msta = MstaFlags::DONE;
    rec.pos.val = 5.0;
    rec.pos.dval = 5.0;
    rec.pos.drbv = 5.0;
    // The lasts track the established position (C: synced at the
    // dispatch that reached 5.0) — the redefinition below must differ
    // from them to pass the move-block entry gate (2240).
    rec.internal.lval = 5.0;
    rec.internal.ldvl = 5.0;
    rec.put_field("SET", EpicsValue::Short(1)).unwrap();

    rec.put_field("DVAL", EpicsValue::Double(0.0)).unwrap();
    let effects = rec.plan_motion(CommandSource::Set);
    assert!(
        effects
            .commands
            .iter()
            .any(|c| matches!(c, MotorCommand::SetPosition { position } if *position == 0.0)),
        "LOAD_POS dispatched at the redefined DVAL"
    );
    assert!(effects.request_poll, "C: LOAD_POS is followed by GET_INFO");
    assert_eq!(rec.stat.mip, MipFlags::LOAD_P);
    assert!(!rec.stat.dmov, "DMOV pulses low during the load");

    // GET_INFO callback: driver reports done at the new position.
    let status = asyn_rs::interfaces::motor::MotorStatus {
        position: 0.0,
        done: true,
        ..Default::default()
    };
    rec.process_motor_info(&status);
    let _ = rec.check_completion();
    assert!(rec.stat.mip.is_empty(), "C 1404-1409: LOAD_P -> DONE");
    assert!(rec.stat.dmov, "DMOV returns TRUE on the status callback");
}

#[test]
fn test_set_mode_tweak_redefinition_retranslates_user_limits() {
    // The tweak fold in SET mode runs the same VAL redefinition (C
    // 2167-2181 feeding 2204-2227) — limits follow the offset.
    let mut rec = MotorRecord::new();
    rec.conv.mres = 1.0;
    rec.stat.msta = MstaFlags::DONE;
    rec.put_field("DHLM", EpicsValue::Double(10.0)).unwrap();
    rec.put_field("DLLM", EpicsValue::Double(-10.0)).unwrap();
    rec.pos.val = 0.0;
    rec.pos.dval = 0.0;
    rec.put_field("SET", EpicsValue::Short(1)).unwrap();
    rec.ctrl.twv = 2.0;

    rec.ctrl.twf = true;
    rec.plan_motion(CommandSource::Twf);
    assert_eq!(rec.pos.off, 2.0, "tweak fold redefined the offset");
    assert_eq!(rec.limits.hlm, 12.0);
    assert_eq!(rec.limits.llm, -8.0);
}

#[test]
fn test_set_dval_under_pause_defers_load_pos_to_go() {
    // C 2237: stop_or_pause returns before the move block, so a
    // SET-mode DVAL written under Pause keeps its dval != ldvl signal
    // and the Go pass's set test (2257-2263) dispatches the load_pos.
    let mut rec = MotorRecord::new();
    rec.conv.mres = 1.0;
    rec.stat.msta = MstaFlags::DONE;
    rec.pos.val = 5.0;
    rec.pos.dval = 5.0;
    rec.pos.drbv = 5.0;
    rec.internal.ldvl = 5.0;
    rec.internal.lval = 5.0;
    rec.put_field("SET", EpicsValue::Short(1)).unwrap();

    rec.ctrl.spmg = SpmgMode::Pause;
    rec.plan_motion(CommandSource::Spmg);

    rec.put_field("DVAL", EpicsValue::Double(0.0)).unwrap();
    let effects = rec.plan_motion(CommandSource::Set);
    assert!(
        effects.commands.is_empty(),
        "C 2237: no LOAD_POS while paused"
    );
    assert!(
        !rec.stat.mip.contains(MipFlags::LOAD_P),
        "the redefinition stays pending, not in flight"
    );

    rec.ctrl.spmg = SpmgMode::Go;
    let effects = rec.plan_motion(CommandSource::Spmg);
    assert!(
        effects
            .commands
            .iter()
            .any(|c| matches!(c, MotorCommand::SetPosition { position } if *position == 0.0)),
        "Go dispatches the deferred load_pos"
    );
    assert_eq!(rec.stat.mip, MipFlags::LOAD_P);
    assert!(!rec.stat.dmov, "DMOV pulses low during the load");
    assert!(effects.request_poll, "C: LOAD_POS is followed by GET_INFO");
}

#[test]
fn test_set_frozen_tweak_under_pause_defers_load_pos_to_go() {
    // The tweak fold is collected under stop_or_pause (C 2167-2181
    // precede the 2237 return) and propagates VAL -> DVAL, but the
    // load_pos dispatch belongs to the move block — it must wait for
    // the Go pass, not fire from the fold.
    let mut rec = MotorRecord::new();
    rec.conv.mres = 1.0;
    rec.stat.msta = MstaFlags::DONE;
    rec.conv.foff = FreezeOffset::Frozen;
    rec.internal.ldvl = 0.0;
    rec.put_field("SET", EpicsValue::Short(1)).unwrap();
    rec.ctrl.twv = 2.0;

    rec.ctrl.spmg = SpmgMode::Pause;
    rec.plan_motion(CommandSource::Spmg);

    rec.ctrl.twf = true;
    let effects = rec.plan_motion(CommandSource::Twf);
    assert!(
        effects.commands.is_empty(),
        "paused fold collects, never dispatches"
    );
    assert_eq!(rec.pos.dval, 2.0, "fold propagated VAL -> DVAL");
    assert!(!rec.stat.mip.contains(MipFlags::LOAD_P));

    rec.ctrl.spmg = SpmgMode::Go;
    let effects = rec.plan_motion(CommandSource::Spmg);
    assert!(
        effects
            .commands
            .iter()
            .any(|c| matches!(c, MotorCommand::SetPosition { position } if *position == 2.0)),
        "Go dispatches the deferred load_pos at the folded target"
    );
    assert_eq!(rec.stat.mip, MipFlags::LOAD_P);
}

#[test]
fn test_rlv_in_set_mode_redefines_offset_without_motion() {
    // C 2187-2193 fold + 2204-2227 collection: an RLV written in SET
    // mode (FOFF=Variable) redefines the offset like a direct VAL
    // write — DVAL untouched, no command, completes on the spot.
    let mut rec = MotorRecord::new();
    rec.conv.mres = 1.0;
    rec.stat.msta = MstaFlags::DONE;
    rec.pos.val = 5.0;
    rec.pos.dval = 5.0;
    rec.pos.drbv = 5.0;
    rec.put_field("SET", EpicsValue::Short(1)).unwrap();

    rec.put_field("RLV", EpicsValue::Double(2.0)).unwrap();
    let effects = rec.plan_motion(CommandSource::Rlv);
    assert!(effects.commands.is_empty(), "no motion, no LOAD_POS");
    assert_eq!(rec.pos.val, 7.0, "RLV folded into VAL");
    assert_eq!(rec.pos.rlv, 0.0, "RLV consumed by the fold");
    assert_eq!(rec.pos.dval, 5.0, "DVAL untouched");
    assert_eq!(rec.pos.off, 2.0, "offset adopts the redefinition");
    assert!(rec.stat.dmov);
    assert!(rec.stat.mip.is_empty());
}

#[test]
fn test_sset_suse_fof_vof_momentary_commands() {
    // C special (motorRecord.cc:2963-2984): SSET/SUSE force SET to
    // Set/Use and FOF/VOF force FOFF to Frozen/Variable on ANY write —
    // the handler never looks at the written value, so a 0 write
    // triggers too.
    let mut rec = MotorRecord::new();
    rec.stat.msta = MstaFlags::DONE;
    rec.internal.init_invariants_synced = true;

    rec.put_field("SSET", EpicsValue::Short(0)).unwrap();
    assert!(rec.conv.set, "any SSET write forces SET mode");
    rec.put_field("SUSE", EpicsValue::Short(0)).unwrap();
    assert!(!rec.conv.set, "any SUSE write forces USE mode");

    rec.put_field("FOF", EpicsValue::Short(0)).unwrap();
    assert_eq!(rec.conv.foff, FreezeOffset::Frozen, "FOF freezes FOFF");
    rec.put_field("VOF", EpicsValue::Short(0)).unwrap();
    assert_eq!(rec.conv.foff, FreezeOffset::Variable, "VOF unfreezes FOFF");

    // The cells echo the last written value, like the C fields the
    // special() handler leaves untouched.
    rec.put_field("SSET", EpicsValue::Short(7)).unwrap();
    assert_eq!(rec.get_field("SSET"), Some(EpicsValue::Short(7)));
}

#[test]
fn test_runtime_mres_change_use_mode_reanchors_with_load_pos() {
    // C do_work (motorRecord.cc:1936-1991): the pp(TRUE) process pass
    // after a runtime MRES write re-anchors a USE-mode record via
    // load_pos — LOAD_POS at the current DVAL, MIP_LOAD_P, DMOV low,
    // GET_INFO.
    let mut rec = MotorRecord::new();
    rec.stat.msta = MstaFlags::DONE;
    rec.internal.init_invariants_synced = true;

    rec.put_field("MRES", EpicsValue::Double(0.5)).unwrap();
    let effects = rec.do_process();
    assert!(
        effects
            .commands
            .iter()
            .any(|c| matches!(c, MotorCommand::SetPosition { .. })),
        "USE-mode re-anchor dispatches LOAD_POS"
    );
    assert!(effects.request_poll, "C: LOAD_POS is followed by GET_INFO");
    assert_eq!(rec.stat.mip, MipFlags::LOAD_P);
    assert!(!rec.stat.dmov, "DMOV pulses low during the load");
    assert!(
        !rec.internal.res_reanchor,
        "the mark dies with its process pass"
    );
}

#[test]
fn test_runtime_mres_change_set_mode_resyncs_drive_values() {
    // C do_work 1980-1986: in SET mode the resolution change arms pp
    // and sends GET_INFO — no LOAD_POS. The status callback then runs
    // postProcess (process 1396-1402 -> 826-849), re-deriving the
    // drive values from the readback at the new resolution.
    let mut rec = MotorRecord::new();
    rec.stat.msta = MstaFlags::DONE;
    rec.internal.init_invariants_synced = true;
    rec.pos.val = 5.0;
    rec.pos.dval = 5.0;
    rec.pos.drbv = 5.0;
    rec.put_field("SET", EpicsValue::Short(1)).unwrap();

    rec.put_field("MRES", EpicsValue::Double(0.5)).unwrap();
    let effects = rec.do_process();
    assert!(
        effects.commands.is_empty(),
        "SET mode re-anchors without LOAD_POS"
    );
    assert!(effects.request_poll, "C 1983-1985: GET_INFO");
    assert!(rec.internal.pp, "C 1982: pp = TRUE");
    assert!(rec.stat.dmov, "no motion: DMOV stays TRUE");

    // GET_INFO callback: readback now reports in the new resolution.
    let status = asyn_rs::interfaces::motor::MotorStatus {
        position: 4.0,
        done: true,
        ..Default::default()
    };
    rec.process_motor_info(&status);
    let _ = rec.check_completion();
    assert!(!rec.internal.pp, "postProcess consumed pp");
    assert_eq!(rec.pos.dval, 4.0, "DVAL re-synced from the readback");
    assert_eq!(rec.pos.val, rec.pos.rbv, "VAL re-synced from RBV");
}

#[test]
fn test_plain_ueip_change_does_not_reanchor() {
    // C special UEIP (2934-2950): a plain UEIP change never MARKs
    // M_UEIP — only the no-encoder override does — so the do_work
    // re-anchor must NOT fire. URIP forcing UEIP off (2955-2959) DOES
    // mark and re-anchors.
    let mut rec = MotorRecord::new();
    rec.stat.msta = MstaFlags::DONE | MstaFlags::ENCODER_PRESENT;
    rec.internal.init_invariants_synced = true;

    rec.put_field("UEIP", EpicsValue::Short(1)).unwrap();
    assert!(rec.conv.ueip);
    let effects = rec.do_process();
    assert!(
        effects.commands.is_empty(),
        "plain UEIP change: no LOAD_POS in C"
    );
    assert!(rec.stat.mip.is_empty());

    // URIP=Yes forces UEIP off — that override path marks M_UEIP.
    rec.put_field("URIP", EpicsValue::Short(1)).unwrap();
    assert!(!rec.conv.ueip);
    let effects = rec.do_process();
    assert!(
        effects
            .commands
            .iter()
            .any(|c| matches!(c, MotorCommand::SetPosition { .. })),
        "UEIP forced off re-anchors via load_pos"
    );
    assert_eq!(rec.stat.mip, MipFlags::LOAD_P);
}

#[test]
fn test_db_loaded_ueip_survives_until_first_poll() {
    // C applies .db field() values as raw struct writes (special() never
    // runs during dbLoadRecords), so field(UEIP,"Yes") survives load even
    // though MSTA is still empty. The first poll performs the encoder
    // check (C 3671-3675) and demotes UEIP without MARKing M_UEIP.
    let mut rec = MotorRecord::new();
    rec.conv.mres = 0.001;

    rec.put_field("UEIP", EpicsValue::Short(1)).unwrap();
    assert!(rec.conv.ueip, "load-time UEIP=Yes stored raw, not vetoed");

    let no_encoder = asyn_rs::interfaces::motor::MotorStatus {
        position: 10.0,
        done: true,
        ..Default::default()
    };
    rec.process_motor_info(&no_encoder);
    assert!(!rec.conv.ueip, "first poll without encoder demotes UEIP");
    assert!(
        !rec.internal.res_reanchor,
        "C 3671-3675 posts the demotion without MARK — no re-anchor"
    );

    // After init, a runtime put repeats the C special() veto (2947-2948):
    // UEIP=Yes with no encoder is overridden back and MARKs M_UEIP.
    rec.internal.init_invariants_synced = true;
    rec.put_field("UEIP", EpicsValue::Short(1)).unwrap();
    assert!(!rec.conv.ueip, "runtime no-encoder put is vetoed");
    assert!(rec.internal.res_reanchor, "the veto path MARKs M_UEIP");
}

#[test]
fn test_db_loaded_ueip_kept_when_encoder_present() {
    let mut rec = MotorRecord::new();
    rec.conv.mres = 0.001;
    rec.conv.eres = 0.002;

    rec.put_field("UEIP", EpicsValue::Short(1)).unwrap();
    let status = asyn_rs::interfaces::motor::MotorStatus {
        position: 10.0,
        encoder_position: 10.0,
        done: true,
        has_encoder: true,
        ..Default::default()
    };
    rec.process_motor_info(&status);
    assert!(rec.conv.ueip, "encoder present: loaded UEIP=Yes is kept");
    assert_eq!(rec.pos.rrbv, 5000, "readback follows the encoder (REP)");
}

#[test]
fn test_db_loaded_urip_does_not_kill_ueip() {
    // C special URIP (2955-2959) is runtime-only: a .db loading both
    // UEIP=Yes and URIP=Yes stores both raw; the poll readback checks
    // UEIP first (C 3676), so UEIP wins while the encoder is present.
    let mut rec = MotorRecord::new();
    rec.put_field("UEIP", EpicsValue::Short(1)).unwrap();
    rec.put_field("URIP", EpicsValue::Short(1)).unwrap();
    assert!(rec.conv.ueip, "load-time URIP write leaves UEIP alone");
    assert!(rec.conv.urip);

    // At runtime the special interplay applies: URIP=Yes forces UEIP=No.
    rec.internal.init_invariants_synced = true;
    rec.put_field("URIP", EpicsValue::Short(1)).unwrap();
    assert!(!rec.conv.ueip, "runtime URIP=Yes forces UEIP off");
}

#[test]
fn test_sset_fof_load_time_writes_store_raw() {
    // C special SSET/SUSE/FOF/VOF (2963-2984) never runs during
    // dbLoadRecords — a load-time write stores the short without
    // forcing SET/FOFF.
    let mut rec = MotorRecord::new();

    rec.put_field("SSET", EpicsValue::Short(1)).unwrap();
    assert!(!rec.conv.set, "load-time SSET does not force SET mode");
    assert_eq!(rec.get_field("SSET"), Some(EpicsValue::Short(1)));

    rec.put_field("FOF", EpicsValue::Short(1)).unwrap();
    assert_eq!(
        rec.conv.foff,
        FreezeOffset::Variable,
        "load-time FOF does not force FOFF (dbd default Variable)"
    );
    assert_eq!(rec.get_field("FOF"), Some(EpicsValue::Short(1)));
}

#[test]
fn test_encoder_loss_demotes_ueip_on_poll() {
    // C overwrites pmr->msta wholesale each poll, so EA_PRESENT is pure
    // driver truth — an axis that stops reporting its encoder demotes
    // UEIP on the next poll (3671-3675). The record must not latch the
    // bit and keep the encoder path alive.
    let mut rec = MotorRecord::new();
    rec.conv.mres = 0.001;
    rec.conv.eres = 0.002;
    rec.conv.ueip = true;

    let with_encoder = asyn_rs::interfaces::motor::MotorStatus {
        position: 10.0,
        encoder_position: 10.0,
        done: true,
        has_encoder: true,
        ..Default::default()
    };
    rec.process_motor_info(&with_encoder);
    assert!(rec.conv.ueip);
    assert!(rec.stat.msta.contains(MstaFlags::ENCODER_PRESENT));

    let lost_encoder = asyn_rs::interfaces::motor::MotorStatus {
        position: 10.0,
        encoder_position: 10.0,
        done: true,
        has_encoder: false,
        ..Default::default()
    };
    rec.process_motor_info(&lost_encoder);
    assert!(!rec.conv.ueip, "encoder loss demotes UEIP");
    assert!(
        !rec.stat.msta.contains(MstaFlags::ENCODER_PRESENT),
        "EA_PRESENT follows the driver, not a record-side latch"
    );
    assert_eq!(rec.pos.rrbv, 10000, "readback fell back to the motor path");
}

#[test]
fn test_mres_reanchor_mark_dies_on_stop_pass() {
    // C mmap marks live one process pass: when the pass takes the
    // do_work top-block stop return (1866-1911), monitor() clears the
    // mark and the re-anchor never fires for that write.
    let mut rec = MotorRecord::new();
    rec.stat.msta = MstaFlags::DONE;
    rec.internal.init_invariants_synced = true;

    rec.put_field("MRES", EpicsValue::Double(0.5)).unwrap();
    rec.ctrl.stop = true; // latent stop pulse rides the same pass
    let effects = rec.do_process();
    assert!(
        !effects
            .commands
            .iter()
            .any(|c| matches!(c, MotorCommand::SetPosition { .. })),
        "stop wins the pass; no LOAD_POS"
    );
    assert!(
        !rec.internal.res_reanchor,
        "the mark dies with the pass, like C monitor() clearing mmap"
    );
}

#[test]
fn test_blocked_homf_stays_latched_and_zero_write_unlatches() {
    // C home gate (2013-2014): a HOMF blocked by its limit switch
    // simply fails the gate — the button stays latched (no clear, no
    // DMOV change). A HOMF=0 put writes the field and un-latches it.
    let mut rec = MotorRecord::new();
    rec.stat.msta = MstaFlags::DONE;
    rec.limits.hls = true;

    rec.put_field("HOMF", EpicsValue::Short(1)).unwrap();
    let effects = rec.plan_motion(CommandSource::Homf);
    assert!(effects.commands.is_empty(), "blocked home must not fire");
    assert!(rec.ctrl.homf, "button stays latched like C's failed gate");
    assert!(rec.stat.dmov, "no DMOV drop outside the gate");
    assert!(rec.stat.mip.is_empty());

    rec.put_field("HOMF", EpicsValue::Short(0)).unwrap();
    assert!(!rec.ctrl.homf, "a 0-write un-latches the parked button");
}

#[test]
fn test_pause_during_active_home_clears_buttons() {
    // C 1899-1900: `if (mip & MIP_HOME) clear_buttons()` — the Pause
    // path cancels an ACTIVE home's latched button, not only a queued
    // one, so Go must not re-dispatch the home.
    let mut rec = MotorRecord::new();
    rec.stat.msta = MstaFlags::DONE;
    rec.put_field("HOMF", EpicsValue::Short(1)).unwrap();
    rec.plan_motion(CommandSource::Homf);
    assert!(rec.stat.mip.contains(MipFlags::HOMF));
    assert!(rec.ctrl.homf, "button reads 1 during the home");

    rec.ctrl.spmg = SpmgMode::Pause;
    rec.plan_motion(CommandSource::Spmg);
    assert!(!rec.ctrl.homf, "Pause cancels the active home's button");
    assert_eq!(rec.stat.mip, MipFlags::STOP);

    // Axis stops; pp armed by the home dispatch syncs and finalizes.
    rec.stat.msta = MstaFlags::DONE;
    rec.stat.movn = false;
    rec.check_completion();

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
fn test_homf_while_paused_drops_dmov_and_stays_latched() {
    // C home section under stop_or_pause (motorRecord.cc:2015-2020): a
    // home button latched while SPMG is Stop/Pause only drops DMOV —
    // no MIP change, no dispatch, button stays latched.
    let mut rec = MotorRecord::new();
    rec.stat.msta = MstaFlags::DONE;
    rec.ctrl.spmg = SpmgMode::Pause;
    rec.plan_motion(CommandSource::Spmg);
    assert!(rec.stat.dmov);

    rec.put_field("HOMF", EpicsValue::Short(1)).unwrap();
    let effects = rec.plan_motion(CommandSource::Homf);
    assert!(!rec.stat.dmov, "C 2017: dmov = FALSE");
    assert!(!rec.stat.mip.contains(MipFlags::HOMF), "no MIP change");
    assert!(rec.ctrl.homf, "button stays latched for the Go pass");
    assert!(
        !effects
            .commands
            .iter()
            .any(|c| matches!(c, MotorCommand::Home { .. })),
        "no dispatch while paused"
    );
}

#[test]
fn test_paused_homf_dispatches_at_go() {
    // C: the Go pass falls through the top block into the home section
    // (2010-2076), which fires the still-latched button.
    let mut rec = MotorRecord::new();
    rec.stat.msta = MstaFlags::DONE;
    rec.ctrl.spmg = SpmgMode::Pause;
    rec.plan_motion(CommandSource::Spmg);
    rec.put_field("HOMF", EpicsValue::Short(1)).unwrap();
    rec.plan_motion(CommandSource::Homf);

    rec.ctrl.spmg = SpmgMode::Go;
    let effects = rec.plan_motion(CommandSource::Spmg);
    assert!(
        effects
            .commands
            .iter()
            .any(|c| matches!(c, MotorCommand::Home { forward: true, .. })),
        "Go dispatches the home latched while paused"
    );
    assert!(rec.stat.mip.contains(MipFlags::HOMF));
    assert_eq!(rec.stat.phase, MotionPhase::Homing);
    assert!(!rec.stat.dmov);
}

#[test]
fn test_paused_homf_wins_over_latched_jog_at_go() {
    // C do_work order: the home section (2010) precedes the jog
    // dispatch and overwrites MIP wholesale (2023) — with both a home
    // and a jog button latched, the Go pass homes.
    let mut rec = MotorRecord::new();
    rec.stat.msta = MstaFlags::DONE;
    rec.ctrl.spmg = SpmgMode::Pause;
    rec.plan_motion(CommandSource::Spmg);
    rec.put_field("HOMF", EpicsValue::Short(1)).unwrap();
    rec.plan_motion(CommandSource::Homf);
    rec.ctrl.jogf = true;

    rec.ctrl.spmg = SpmgMode::Go;
    let effects = rec.plan_motion(CommandSource::Spmg);
    assert!(
        effects
            .commands
            .iter()
            .any(|c| matches!(c, MotorCommand::Home { forward: true, .. })),
        "home wins the Go pass"
    );
    assert!(
        !effects
            .commands
            .iter()
            .any(|c| matches!(c, MotorCommand::MoveVelocity { .. })),
        "latched jog does not dispatch on the same pass"
    );
}

#[test]
fn test_homf_homr_puts_rejected_while_home_in_flight() {
    // C special() (motorRecord.cc:2610-2614): a HOMF/HOMR put while
    // mip & MIP_HOME returns ERROR — button not written, no processing.
    let mut rec = MotorRecord::new();
    rec.stat.msta = MstaFlags::DONE;
    assert!(rec.put_field("HOMF", EpicsValue::Short(1)).is_ok());
    rec.plan_motion(CommandSource::Homf);
    assert!(rec.stat.mip.contains(MipFlags::HOMF));

    assert!(rec.put_field("HOMF", EpicsValue::Short(1)).is_err());
    assert!(rec.put_field("HOMR", EpicsValue::Short(1)).is_err());
    // HOMF reads 1 from its own dispatch latch (C 893-906); the vetoed
    // opposite-direction put must not latch.
    assert!(rec.ctrl.homf, "button reads 1 during its own home");
    assert!(!rec.ctrl.homr, "vetoed put must not latch the button");
    // A vetoed 0-write must not un-latch it either.
    assert!(rec.put_field("HOMF", EpicsValue::Short(0)).is_err());
    assert!(rec.ctrl.homf);
}

#[test]
fn test_homf_put_rejected_while_home_queued_behind_stop() {
    // A queued home reads back as MIP_HOMF|MIP_STOP (C 2023-2033),
    // which still intersects MIP_HOME — the 2610-2614 veto applies.
    let mut rec = MotorRecord::new();
    rec.put_field("HLM", EpicsValue::Double(1000.0)).unwrap();
    rec.put_field("LLM", EpicsValue::Double(-1000.0)).unwrap();
    rec.pos.dval = 50.0;
    rec.plan_motion(CommandSource::Val);
    rec.stat.msta = MstaFlags::MOVING;
    rec.stat.movn = true;

    rec.ctrl.homf = true;
    rec.plan_motion(CommandSource::Homf);
    assert!(rec.stat.mip.contains(MipFlags::HOMF | MipFlags::STOP));

    assert!(rec.put_field("HOMF", EpicsValue::Short(1)).is_err());
    assert!(rec.put_field("HOMR", EpicsValue::Short(1)).is_err());
}

#[test]
fn test_home_accel_derives_from_hvel_not_velo() {
    // C home dispatch (motorRecord.cc:2046-2048) computes the home
    // acceleration from HVEL, not VELO:
    // (hvel - vbase) > 0 ? (hvel - vbase) / accl : hvel / accl.
    let mut rec = MotorRecord::new();
    rec.stat.msta = MstaFlags::DONE;
    rec.vel.vbas = 0.5;
    rec.vel.velo = 5.0;
    rec.vel.hvel = 2.0;
    rec.vel.accl = 0.5;

    rec.ctrl.homf = true;
    let effects = rec.plan_motion(CommandSource::Homf);
    let accel = effects
        .commands
        .iter()
        .find_map(|c| match c {
            MotorCommand::Home { acceleration, .. } => Some(*acceleration),
            _ => None,
        })
        .expect("home command dispatched");
    assert_eq!(accel, (2.0 - 0.5) / 0.5, "HVEL-based, not VELO-based");
}

#[test]
fn test_queued_home_refire_accel_derives_from_hvel() {
    // The stop-completion re-fire (C 859-862) uses the same HVEL-based
    // acceleration as the direct dispatch.
    let mut rec = MotorRecord::new();
    rec.put_field("HLM", EpicsValue::Double(1000.0)).unwrap();
    rec.put_field("LLM", EpicsValue::Double(-1000.0)).unwrap();
    rec.vel.vbas = 0.5;
    rec.vel.velo = 5.0;
    rec.vel.hvel = 2.0;
    rec.vel.accl = 0.5;
    rec.pos.dval = 50.0;
    rec.plan_motion(CommandSource::Val);
    rec.stat.msta = MstaFlags::MOVING;
    rec.stat.movn = true;

    rec.ctrl.homf = true;
    rec.plan_motion(CommandSource::Homf);
    assert!(rec.internal.queued_motion.is_some());

    rec.stat.msta = MstaFlags::DONE;
    rec.stat.movn = false;
    let effects = rec.check_completion();
    let accel = effects
        .commands
        .iter()
        .find_map(|c| match c {
            MotorCommand::Home { acceleration, .. } => Some(*acceleration),
            _ => None,
        })
        .expect("queued home re-fired at stop completion");
    assert_eq!(accel, (2.0 - 0.5) / 0.5, "HVEL-based, not VELO-based");
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
fn test_tweak_folds_val_with_the_user_direction_hard_limit_active() {
    // C motorRecord.cc:2167-2181: the tweak fold is unconditional — `val += twv
    // * dir` with no hls/lls test — so a TWF struck against the active high
    // limit still lands in VAL (and DVAL), exactly like the direct VAL write it
    // is shorthand for. The port gated the fold on the hard limit and consumed
    // the button silently, leaving VAL at its old value.
    let mut rec = MotorRecord::new();
    rec.conv.mres = 0.01;
    rec.limits.dhlm = 100.0;
    rec.limits.dllm = -100.0;
    rec.stat.msta = MstaFlags::DONE;
    rec.ctrl.twv = 5.0;
    rec.pos.val = 10.0;
    rec.pos.dval = 10.0;
    rec.limits.hls = true; // + direction switch active; soft target still legal

    rec.ctrl.twf = true;
    let _ = rec.plan_motion(CommandSource::Twf);

    assert_eq!(
        rec.pos.val, 15.0,
        "C folds VAL regardless of the limit switch"
    );
    assert_eq!(rec.pos.dval, 15.0, "the fold cascades VAL -> DVAL");
    assert!(!rec.ctrl.twf, "the button releases either way");

    // Reverse tweak away from the struck high limit is unaffected.
    rec.ctrl.twr = true;
    let _ = rec.plan_motion(CommandSource::Twr);
    assert_eq!(rec.pos.val, 10.0);
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
    // C 2206-2227: the Variable-offset redefinition sends NOTHING — no
    // move and no LOAD_POS — and completes on the spot.
    assert!(
        effects.commands.is_empty(),
        "SET+FOFF=Variable tweak redefines without any controller command"
    );
    assert!(rec.stat.dmov);
}

#[test]
fn test_tweak_blocked_when_loadpos_blocked_in_set_mode() {
    // The #231 LOAD_POS block applies only to the FOFF=Frozen leg,
    // whose redefinition needs the controller write (C load_pos sends
    // LOAD_POS unconditionally, 3811).
    let mut rec = MotorRecord::new();
    rec.conv.mres = 0.01;
    rec.stat.msta = MstaFlags::DONE;
    rec.conv.set = true;
    rec.conv.foff = FreezeOffset::Frozen;
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
fn test_tweak_offset_only_escapes_loadpos_block_in_set_mode() {
    // C 2206-2227: the SET+FOFF=Variable tweak fold is an offset-only
    // redefinition — no controller command — so the #231 LOAD_POS
    // block must not refuse it.
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
    assert_eq!(rec.pos.val, 15.0, "fold lands in VAL");
    assert_eq!(rec.pos.dval, 10.0, "DVAL untouched (offset-only)");
    assert_eq!(rec.pos.off, 5.0, "OFF absorbs the redefinition");
    assert!(
        effects.commands.is_empty(),
        "no controller command from the offset-only redefinition"
    );
}

#[test]
fn test_field_list_coverage() {
    let rec = MotorRecord::new();
    let ungettable: Vec<&str> = rec
        .field_list()
        .iter()
        .filter(|fd| rec.get_field(fd.name).is_none())
        .map(|fd| fd.name)
        .collect();
    assert_eq!(ungettable, UNIMPLEMENTED);
}

/// The fields `motorRecord.dbd` declares that this port does not implement.
///
/// They were invisible while the declaration was a hand-written table: the hand
/// table simply omitted them, so "declared" and "implemented" were the same set
/// by construction and the gap could not be seen. The declaration is the `.dbd`
/// now, so the gap is a list — and it is a RATCHET: a new unimplemented field
/// has to be added here, and implementing one has to remove it.
///
/// What remains is the set `motorRecord.cc` never assigns outside
/// `init_record`, so serving one from record state would give it nothing to
/// track: `CARD` (set once from the OUT link type, `motorRecord.cc:650-665`),
/// `HSV`/`LSV` (declared but read by nothing in `motorRecord.cc` — `alarm_sub`
/// raises both limit alarms at `HLSV`, `:3401-3407`), `INIT` (startup command
/// string, consumed by device support at `motordevCom.cc:310`), `PREM`
/// (pre-move command, consumed by vendor device support, e.g.
/// `devPM304.cc:209-211`) and `LOCK` (soft-channel position lock, read at
/// `devSoft.cc:237`).
///
/// Being on this list is not the same as being unreachable. A client asking
/// this port for one of them gets an answer: `RecordInstance::resolve_field`
/// falls through to `declared_default`, which serves the `.dbd`
/// `initial(...)`, and a put lands in `declared_overrides` and reads back. For
/// a field the record genuinely never writes that IS C's value. That is also
/// why the list has to stay honest — `LSPG` and `PP` sat here while the record
/// was writing them all along, and the fallback answered `3` and `0` forever
/// with nothing to show anything was wrong.
const UNIMPLEMENTED: &[&str] = &["CARD", "HSV", "LSV", "INIT", "PREM", "LOCK"];

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
    assert_eq!(rec.stat.phase, MotionPhase::MainMove);
    assert!(
        rec.stat.mip.contains(MipFlags::RETRY),
        "retry marked in MIP"
    );
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
fn test_ls_stop_syncs_drive_values_to_readback() {
    // C motorRecord.cc:1366-1380 → postProcess (826-849): a positional
    // move halted by a hardware limit switch in the travel direction
    // forces pp=TRUE and re-arms GET_INFO, and the next callback adopts
    // the limit readback into VAL/DVAL/RVAL (DIFF/RDIF zeroed). Without
    // this sync the record advertises the unreached target forever.
    let mut rec = MotorRecord::new();
    rec.stat.msta = MstaFlags::DONE;
    rec.stat.phase = MotionPhase::MainMove;
    rec.conv.mres = 1.0;
    rec.retry.rtry = 3; // retry would fire on a 1.0 error...
    // Commanded the +direction into the high limit; the switch is struck.
    rec.stat.cdir = true;
    rec.limits.hls = true;
    // Target 10, but the axis stopped at the limit (9.0) one EGU short.
    rec.pos.dval = 10.0;
    rec.pos.val = 10.0;
    rec.pos.rval = 10;
    rec.pos.drbv = 9.0;
    rec.pos.rbv = 9.0;

    let _ = rec.check_completion();

    assert_eq!(rec.stat.phase, MotionPhase::Idle, "LS-stop finalizes");
    assert!(
        !rec.stat.mip.contains(MipFlags::RETRY),
        "the limit suppresses the retry"
    );
    assert_eq!(
        rec.pos.val, 9.0,
        "VAL adopts the limit readback (C postProcess)"
    );
    assert_eq!(rec.pos.dval, 9.0, "DVAL adopts the dial limit readback");
    assert_eq!(rec.pos.rval, 9, "RVAL = NINT(DVAL/MRES) at the limit");
    assert_eq!(rec.pos.diff, 0.0, "DIFF zeroed at the limit");
    assert_eq!(rec.pos.rdif, 0, "RDIF zeroed at the limit");
}

#[test]
fn test_ntm_retarget_direction_change() {
    let mut rec = MotorRecord::new();
    rec.timing.ntm = true;
    rec.timing.ntmf = 2;
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
    rec.timing.ntmf = 2;
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
        has_encoder: true,
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
        has_encoder: true,
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
fn test_rep_scales_by_eres_even_under_ueip_no() {
    // C devMotorAsyn.c:459-464: REP is the raw encoder count whether or
    // not the readback uses it — UEIP selects the RRBV source, not the
    // REP scale.
    let mut rec = MotorRecord::new();
    rec.conv.mres = 0.001;
    rec.conv.eres = 0.002;
    rec.conv.ueip = false;
    let status = asyn_rs::interfaces::motor::MotorStatus {
        position: 10.0,
        encoder_position: 10.0,
        done: true,
        ..Default::default()
    };
    rec.process_motor_info(&status);
    assert_eq!(rec.pos.rep, 5000, "REP in encoder counts via ERES");
    assert_eq!(rec.pos.rrbv, 10000, "readback still follows the motor");
    assert_eq!(rec.pos.drbv, 10.0);
}

#[test]
fn test_stup_triggers_status_refresh() {
    let mut rec = MotorRecord::new();
    rec.set_device_state(motor_rs::device_state::new_shared_state());
    rec.stat.stup = 1;
    let effects = rec.do_process();
    assert!(effects.status_refresh);
    // C do_work top (1817-1830): ON transitions to BUSY until the data
    // callback acknowledges (process_exit 1498-1502).
    assert_eq!(rec.stat.stup, 2, "STUP holds BUSY until the callback");
    assert!(effects.commands.is_empty());
}

#[test]
fn test_stup_without_driver_returns_to_off() {
    // C do_work 1824-1828: a device that cannot service GET_INFO
    // (WRITE_MSG returns ERROR, e.g. Soft Channel) drops STUP back to
    // OFF instead of holding BUSY. With no driver the BUSY->OFF
    // callback can never arrive, and a stuck BUSY would refuse every
    // later STUP put.
    let mut rec = MotorRecord::new();
    rec.stat.stup = 1;
    let effects = rec.do_process();
    assert!(!effects.status_refresh, "no consumer for the refresh");
    assert_eq!(rec.stat.stup, 0, "STUP returns to OFF, not stuck BUSY");
    // The field stays writable for a later request.
    rec.put_field("STUP", EpicsValue::Short(1)).unwrap();
    assert_eq!(rec.stat.stup, 1);
}

#[test]
fn test_stup_does_not_drop_concurrent_user_write() {
    // C handles STUP at the top of do_work and continues the pass. A user
    // write (last_write) arriving in the same cycle must still be planned —
    // the STUP must not early-return and discard it.
    let mut rec = MotorRecord::new();
    let state = motor_rs::device_state::new_shared_state();
    rec.set_device_state(state.clone());
    anchor(&mut rec, &state);
    rec.put_field("HLM", EpicsValue::Double(1000.0)).unwrap();
    rec.put_field("LLM", EpicsValue::Double(-1000.0)).unwrap();
    rec.stat.stup = 1;
    // VAL write in the same process cycle as the STUP request.
    rec.put_field("VAL", EpicsValue::Double(100.0)).unwrap();
    rec.set_event(MotorEvent::UserWrite(CommandSource::Val));
    let effects = rec.do_process();
    // STUP honoured…
    assert!(effects.status_refresh);
    assert_eq!(rec.stat.stup, 2, "BUSY until the callback");
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
fn test_stup_put_rejects_non_on_and_busy_retrigger() {
    // C special (3084-3090): a written value other than ON is forced
    // back to OFF and does not trigger the protocol; (2615-2617): a put
    // while the previous request is in flight is refused.
    let mut rec = MotorRecord::new();
    let state = motor_rs::device_state::new_shared_state();
    rec.set_device_state(state.clone());
    anchor(&mut rec, &state);

    rec.put_field("STUP", EpicsValue::Short(2)).unwrap();
    assert_eq!(rec.stat.stup, 0, "BUSY write forced to OFF");
    // The write pass itself is a NOTHING_DONE pass that dispatches
    // nothing, so C's implicit GET_INFO (2546-2557) fires on it: STUP
    // goes BUSY and a refresh is requested even though the put was
    // forced to OFF — the protocol is triggered by the pass, not by
    // the written value.
    let effects = rec.do_process();
    assert!(
        effects.status_refresh,
        "implicit GET_INFO fires on the no-op put pass"
    );
    assert_eq!(rec.stat.stup, 2, "BUSY until the data callback");
    assert!(
        rec.put_field("STUP", EpicsValue::Short(1)).is_err(),
        "re-trigger while BUSY is refused"
    );

    // The data callback returns BUSY to OFF (C process_exit 1498-1502,
    // ported in determine_event); the field is writable again.
    rec.stat.stup = 0;
    rec.put_field("STUP", EpicsValue::Short(1)).unwrap();
    assert_eq!(rec.stat.stup, 1);
}

#[test]
fn test_idle_status_pass_blocks_implicit_get_info() {
    // C 2546: the implicit GET_INFO arm gates on `proc_ind ==
    // NOTHING_DONE` — a CALLBACK_DATA pass firing it would request a
    // fresh status from every status, an unbounded poll loop. The
    // one-pass mark from determine_event's Idle consume carries that
    // discriminator.
    let mut rec = MotorRecord::new();
    let state = motor_rs::device_state::new_shared_state();
    rec.set_device_state(state.clone());
    anchor(&mut rec, &state);

    rec.internal.idle_status_pass = true;
    let effects = rec.do_process();
    assert!(
        !effects.status_refresh,
        "CALLBACK_DATA pass: no implicit GET_INFO"
    );
    assert_eq!(rec.stat.stup, 0);

    // The same no-op pass without the mark is a put/scan pass — the
    // chain end fires the refresh.
    let effects = rec.do_process();
    assert!(
        effects.status_refresh,
        "put/scan pass fires the implicit GET_INFO"
    );
    assert_eq!(rec.stat.stup, 2, "STUP holds BUSY until the callback");
}

#[test]
fn test_cnen_put_pass_fires_implicit_get_info() {
    // C: CNEN is special(SPC_MOD) + pp(TRUE) — special() owns the torque
    // send (3029-3040, GAIN_SUPPORT gated) and the pp pass runs the full
    // do_work, where nothing dispatches and the chain end fires the
    // implicit GET_INFO (2546-2557). The refresh comes from the pass,
    // not from gain support.
    let mut rec = MotorRecord::new();
    rec.set_device_state(motor_rs::device_state::new_shared_state());

    // Without GAIN_SUPPORT: no torque command, the refresh still fires.
    rec.put_field("CNEN", EpicsValue::Short(1)).unwrap();
    let effects = rec.plan_motion(CommandSource::Cnen);
    assert!(
        effects.commands.is_empty(),
        "no torque send without GAIN_SUPPORT"
    );
    assert!(
        effects.status_refresh,
        "implicit GET_INFO fires on the CNEN put pass"
    );
    assert_eq!(rec.stat.stup, 2);

    // With GAIN_SUPPORT: the torque command plus the same refresh.
    rec.stat.stup = 0; // data callback acked the previous request
    rec.stat.msta.insert(MstaFlags::GAIN_SUPPORT);
    rec.put_field("CNEN", EpicsValue::Short(0)).unwrap();
    let effects = rec.plan_motion(CommandSource::Cnen);
    assert!(
        effects
            .commands
            .iter()
            .any(|c| matches!(c, MotorCommand::SetClosedLoop { enable: false }))
    );
    assert!(effects.status_refresh);
    assert_eq!(rec.stat.stup, 2);
}

#[test]
fn test_spmg_noop_go_flip_fires_implicit_get_info() {
    // C: SPMG is pp(TRUE). A Pause pass returns inside the top block
    // (1898-1911) — consumed, no implicit GET_INFO. A Go written back
    // on an idle in-position axis collapses the bare MIP_STOP, fails
    // the move-block entry (2241: dval == ldvl && dmov) and falls to
    // the chain end — the implicit GET_INFO fires.
    let mut rec = MotorRecord::new();
    rec.set_device_state(motor_rs::device_state::new_shared_state());

    rec.ctrl.spmg = SpmgMode::Pause;
    let effects = rec.plan_motion(CommandSource::Spmg);
    assert!(
        effects
            .commands
            .iter()
            .any(|c| matches!(c, MotorCommand::Stop { .. })),
        "Pause sends the just-in-case stop"
    );
    assert!(
        !effects.status_refresh,
        "Pause pass is consumed in the top block"
    );
    assert_eq!(rec.stat.stup, 0);

    rec.ctrl.spmg = SpmgMode::Go;
    let effects = rec.plan_motion(CommandSource::Spmg);
    assert!(effects.commands.is_empty(), "nothing to resume");
    assert!(
        effects.status_refresh,
        "no-op Go flip falls to the chain end"
    );
    assert_eq!(rec.stat.stup, 2);
}

#[test]
fn test_blocked_home_put_fires_implicit_get_info_button_stays() {
    // C: the home-section entry gate (2013-2014) fails for a direction
    // blocked by its limit switch — the pass falls through the section
    // to the chain end (implicit GET_INFO) and the button stays latched.
    let mut rec = MotorRecord::new();
    rec.set_device_state(motor_rs::device_state::new_shared_state());

    rec.limits.hls = true;
    rec.put_field("HOMF", EpicsValue::Short(1)).unwrap();
    let effects = rec.plan_motion(CommandSource::Homf);
    assert!(
        effects.commands.is_empty(),
        "blocked home dispatches nothing"
    );
    assert!(
        effects.status_refresh,
        "the unconsumed pass falls to the chain end"
    );
    assert!(rec.ctrl.homf, "button stays latched");
    assert_eq!(rec.stat.mip, MipFlags::empty());
}

#[test]
fn test_idle_jog_release_fires_implicit_get_info() {
    // C: a JOGF=0 release on an idle axis matches no jog branch (no
    // MIP_JOGF/JOGR, no queued jog) — the pass falls through to the
    // chain end (2546).
    let mut rec = MotorRecord::new();
    rec.set_device_state(motor_rs::device_state::new_shared_state());

    rec.put_field("JOGF", EpicsValue::Short(0)).unwrap();
    let effects = rec.plan_motion(CommandSource::Jogf);
    assert!(effects.commands.is_empty());
    assert!(
        effects.status_refresh,
        "idle release falls to the chain end"
    );
    assert_eq!(rec.stat.stup, 2);
}

#[test]
fn test_hw_blocked_jog_fires_implicit_get_info_button_stays() {
    // C special (3042-3053): a hardware-blocked direction never arms
    // MIP_JOG_REQ, so the put pass skips the jog section and falls to
    // the chain end; the button stays latched for the latent collection.
    let mut rec = MotorRecord::new();
    rec.set_device_state(motor_rs::device_state::new_shared_state());

    rec.limits.hls = true;
    rec.put_field("JOGF", EpicsValue::Short(1)).unwrap();
    let effects = rec.plan_motion(CommandSource::Jogf);
    assert!(
        effects.commands.is_empty(),
        "blocked jog dispatches nothing"
    );
    assert!(effects.status_refresh);
    assert!(rec.ctrl.jogf, "button stays latched");
    assert_eq!(rec.stat.phase, MotionPhase::Idle);
}

#[test]
fn test_lvio_refused_jog_consumes_pass_no_get_info() {
    // C 2092-2104: the soft-limit jog refusal returns IN-SECTION — the
    // pass is consumed (buttons released, LVIO raised) and never
    // reaches the implicit GET_INFO arm.
    let mut rec = MotorRecord::new();
    rec.set_device_state(motor_rs::device_state::new_shared_state());
    rec.limits.dhlm = 10.0;
    rec.limits.dllm = -10.0;
    rec.limits.hlm = 10.0;
    rec.limits.llm = -10.0;
    rec.vel.jvel = 5.0;
    rec.pos.val = 8.0; // 8.0 > HLM - JVEL = 5.0: forward jog violates

    rec.put_field("JOGF", EpicsValue::Short(1)).unwrap();
    let effects = rec.plan_motion(CommandSource::Jogf);
    assert!(effects.commands.is_empty());
    assert!(
        !effects.status_refresh,
        "in-section refusal consumes the pass"
    );
    assert!(!rec.ctrl.jogf, "refusal releases the buttons");
    assert!(rec.limits.lvio);
    assert_eq!(rec.stat.stup, 0);
}

#[test]
fn test_go_resume_dispatch_consumes_pass_no_get_info() {
    // C: a Go pass the move block consumes (entry true, dispatch at
    // 2455) never reaches the chain end — no implicit GET_INFO.
    let mut rec = MotorRecord::new();
    rec.set_device_state(motor_rs::device_state::new_shared_state());

    rec.ctrl.spmg = SpmgMode::Pause;
    rec.plan_motion(CommandSource::Spmg);
    // Target parked while paused (stop_or_pause returns before the
    // move block).
    rec.put_field("VAL", EpicsValue::Double(50.0)).unwrap();
    rec.ctrl.spmg = SpmgMode::Go;
    let effects = rec.plan_motion(CommandSource::Spmg);
    assert!(
        effects
            .commands
            .iter()
            .any(|c| matches!(c, MotorCommand::MoveAbsolute { .. })),
        "Go resumes the parked move"
    );
    assert!(
        !effects.status_refresh,
        "consumed pass: no implicit GET_INFO"
    );
    assert_eq!(rec.stat.stup, 0);
}

#[test]
fn test_put_pass_leaves_injected_device_update_pending() {
    use asyn_rs::interfaces::motor::MotorStatus;
    // One signal, one pass (C dbScanLock): a put-owned pass must not
    // consume a coalesced DeviceUpdate — the update keeps its
    // completion pipeline on the following pass instead of being
    // reduced to a readback apply.
    let mut rec = MotorRecord::new();
    rec.put_field("VAL", EpicsValue::Double(10.0)).unwrap();
    rec.do_process();
    assert_eq!(rec.stat.phase, MotionPhase::MainMove);

    let done = MotorStatus {
        position: 10.0,
        encoder_position: 10.0,
        done: true,
        ..Default::default()
    };
    rec.put_field("CNEN", EpicsValue::Short(1)).unwrap();
    rec.set_event(MotorEvent::DeviceUpdate(done));
    rec.do_process(); // the put's pass
    assert_eq!(
        rec.stat.phase,
        MotionPhase::MainMove,
        "completion not consumed by the put pass"
    );
    assert!(!rec.stat.dmov);

    rec.do_process(); // the update's own pass
    assert_eq!(rec.stat.phase, MotionPhase::Idle);
    assert!(rec.stat.dmov);
}

#[test]
fn test_put_pass_leaves_delay_expiry_pending() {
    use asyn_rs::interfaces::motor::MotorStatus;
    // A DelayExpired coalesced with a put must survive the put pass:
    // the settle watchdog timer is one-shot, so consuming it without
    // acting would leave DELAY_REQ armed forever.
    let mut rec = MotorRecord::new();
    rec.timing.dly = 0.5;
    rec.pos.dval = 10.0;
    rec.plan_motion(CommandSource::Val);

    let done = MotorStatus {
        position: 10.0,
        encoder_position: 10.0,
        done: true,
        ..Default::default()
    };
    rec.process_motor_info(&done);
    rec.check_completion();
    assert_eq!(rec.stat.phase, MotionPhase::DelayWait);
    assert!(rec.stat.mip.contains(MipFlags::DELAY_REQ));

    rec.put_field("CNEN", EpicsValue::Short(1)).unwrap();
    rec.set_event(MotorEvent::DelayExpired);
    rec.do_process(); // the put's pass
    assert!(
        rec.stat.mip.contains(MipFlags::DELAY_REQ),
        "watchdog survives the put pass"
    );
    assert!(!rec.stat.mip.contains(MipFlags::DELAY_ACK));

    let effects = rec.do_process(); // the expiry's own pass
    assert!(rec.stat.mip.contains(MipFlags::DELAY_ACK));
    assert!(effects.status_refresh);
}

#[test]
fn test_closed_loop_button_put_fires_implicit_get_info() {
    // C 1994-2007: the closed-loop else bypasses only the collection
    // sections (2008-2198); the pass still falls through to the chain
    // end, so a button put that dispatched nothing fires the implicit
    // GET_INFO while the button stays latched for an OMSL flip.
    let mut rec = MotorRecord::new();
    rec.set_device_state(motor_rs::device_state::new_shared_state());
    rec.links.omsl = 1;
    rec.links.dol = "setpoint_src.VAL".to_string();

    rec.put_field("JOGF", EpicsValue::Short(1)).unwrap();
    let effects = rec.plan_motion(CommandSource::Jogf);
    assert!(effects.commands.is_empty(), "no jog under closed loop");
    assert!(
        effects.status_refresh,
        "the bypassed pass still reaches the chain end"
    );
    assert!(rec.ctrl.jogf, "button stays latched");
    assert_eq!(rec.stat.stup, 2);
}

#[test]
fn test_stup_busy_ack_callback_skips_completion() {
    use asyn_rs::interfaces::motor::MotorStatus;
    // C 1345: the data callback that acknowledges a BUSY STUP takes
    // neither the moving nor the motor-stopped branch — the GET_INFO
    // response must not complete an in-flight motion; the next poll
    // does.
    let mut rec = MotorRecord::new();
    rec.put_field("VAL", EpicsValue::Double(10.0)).unwrap();
    rec.plan_motion(CommandSource::Val);
    assert_eq!(rec.stat.phase, MotionPhase::MainMove);
    assert!(!rec.stat.dmov);

    let done = MotorStatus {
        position: 10.0,
        encoder_position: 10.0,
        done: true,
        ..Default::default()
    };
    // The ack pass: determine_event returned BUSY to OFF and latched
    // the one-pass mark.
    rec.internal.stup_ack = true;
    rec.set_event(MotorEvent::DeviceUpdate(done.clone()));
    rec.do_process();
    assert_eq!(
        rec.stat.phase,
        MotionPhase::MainMove,
        "ack callback does not complete the motion"
    );
    assert!(!rec.stat.dmov);

    // The next ordinary poll completes it.
    rec.set_event(MotorEvent::DeviceUpdate(done));
    rec.do_process();
    assert_eq!(rec.stat.phase, MotionPhase::Idle);
    assert!(rec.stat.dmov);
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
fn test_cnen_tracks_ea_position_under_gain_support() {
    use asyn_rs::interfaces::motor::MotorStatus;
    // C monitor() (3541-3549): a GAIN_SUPPORT controller syncs CNEN
    // from the EA_POSITION torque readback on every MSTA post, so an
    // externally toggled torque shows up in the field.
    let mut rec = MotorRecord::new();
    rec.conv.mres = 1.0;

    let torque_on = MotorStatus {
        done: true,
        gain_support: true,
        powered: true,
        ..Default::default()
    };
    rec.process_motor_info(&torque_on);
    assert!(rec.ctrl.cnen, "EA_POSITION on syncs CNEN=Enable");

    let torque_off = MotorStatus {
        done: true,
        gain_support: true,
        powered: false,
        ..Default::default()
    };
    rec.process_motor_info(&torque_off);
    assert!(!rec.ctrl.cnen, "EA_POSITION off syncs CNEN=Disable");

    // Without GAIN_SUPPORT the user's CNEN is left alone.
    rec.ctrl.cnen = true;
    let no_gain = MotorStatus {
        done: true,
        gain_support: false,
        powered: false,
        ..Default::default()
    };
    rec.process_motor_info(&no_gain);
    assert!(rec.ctrl.cnen, "CNEN untouched without GAIN_SUPPORT");
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
    // Armed-but-undispatched retry (paused): mip = MIP_RETRY with the
    // axis at rest. An in-flight retry would be MainMove + RETRY|MOVE.
    rec.stat.phase = MotionPhase::Idle;
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
fn test_spmg_pause_with_queued_home_clears_latched_jog_button() {
    // C 1899-1900: `if (mip & MIP_HOME) clear_buttons()` releases ALL
    // FOUR buttons (4386-4408) — a jog button latched while a home is
    // queued dies with the home, and Go resumes neither.
    let mut rec = MotorRecord::new();
    rec.put_field("HLM", EpicsValue::Double(1000.0)).unwrap();
    rec.put_field("LLM", EpicsValue::Double(-1000.0)).unwrap();
    rec.pos.dval = 50.0;
    rec.plan_motion(CommandSource::Val);
    rec.stat.msta = MstaFlags::MOVING;
    rec.stat.movn = true;

    rec.ctrl.homf = true;
    rec.plan_motion(CommandSource::Homf);
    assert!(rec.stat.mip.contains(MipFlags::HOMF));
    rec.ctrl.jogf = true; // latched behind the queued home

    rec.ctrl.spmg = SpmgMode::Pause;
    rec.plan_motion(CommandSource::Spmg);
    assert!(!rec.ctrl.homf, "home button dropped (C clear_buttons)");
    assert!(
        !rec.ctrl.jogf,
        "C clear_buttons (1899-1900) drops the latched jog button too"
    );

    // Stop completes; Go resumes neither the home nor the jog.
    rec.stat.msta = MstaFlags::DONE;
    rec.stat.movn = false;
    let _ = rec.check_completion();
    rec.internal.lspg = SpmgMode::Pause;
    rec.ctrl.spmg = SpmgMode::Go;
    let effects = rec.plan_motion(CommandSource::Spmg);
    assert!(
        !effects.commands.iter().any(|c| matches!(
            c,
            MotorCommand::Home { .. } | MotorCommand::MoveVelocity { .. }
        )),
        "Go after Pause resumes nothing — every button was cleared"
    );
}

#[test]
fn test_val_during_home_clears_latched_jog_button() {
    // C 1388-1393 (0aaf02d7, PR #224): a VAL written during a home
    // calls clear_buttons() — ALL FOUR buttons — so a jog latched
    // behind the home cannot fire after the retarget replays.
    let mut rec = MotorRecord::new();
    rec.put_field("HLM", EpicsValue::Double(1000.0)).unwrap();
    rec.put_field("LLM", EpicsValue::Double(-1000.0)).unwrap();
    rec.stat.msta = MstaFlags::DONE;
    rec.put_field("HOMF", EpicsValue::Short(1)).unwrap();
    rec.plan_motion(CommandSource::Homf);
    assert!(rec.stat.mip.contains(MipFlags::HOMF));
    rec.ctrl.jogf = true; // latched during the home, not yet dispatched

    rec.put_field("VAL", EpicsValue::Double(5.0)).unwrap();
    rec.plan_motion(CommandSource::Val);
    assert!(!rec.ctrl.homf, "VAL during home releases the home button");
    assert!(
        !rec.ctrl.jogf,
        "C clear_buttons (1392) drops the latched jog button too"
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
    assert!(rec.ctrl.homf, "button reads 1 during the home (C 893-906)");
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
    // C dbd: ACCS has no initial() — it loads 0.0 and derives from ACCL
    // at init ((velo=0.0 - vbas=0.0) / accl=0.2 == 0.0).
    assert!((rec.vel.accs - 0.0).abs() < 1e-12);
}

#[test]
fn test_put_accl_recalculates_accs_and_leaves_accu_untouched() {
    // C special() motorRecordACCL (motorRecord.cc:2735-2742): recompute ACCS
    // from ACCL but DO NOT change ACCU (63bfe5d0 made ACCU a user control).
    let mut rec = MotorRecord::new();
    // The accel cross-calcs are C special() — runtime semantics, so init first
    // (a load leaves ACCL/ACCS raw; see motor_sync_speed_at_init).
    rec.init_record(0).unwrap();
    rec.init_record(1).unwrap();
    rec.vel.velo = 10.0;
    rec.vel.vbas = 0.0;
    rec.vel.accu = AccsUsed::Accs; // start from Accs — must survive the put
    rec.put_field("ACCL", EpicsValue::Double(2.0)).unwrap();
    assert_eq!(rec.vel.accu, AccsUsed::Accs);
    assert!((rec.vel.accl - 2.0).abs() < 1e-12);
    // ACCS = (10 - 0) / 2 = 5
    assert!((rec.vel.accs - 5.0).abs() < 1e-12);
}

#[test]
fn test_put_accs_recalculates_accl_and_leaves_accu_untouched() {
    // C special() motorRecordACCS (motorRecord.cc:2745-2752): recompute ACCL
    // from ACCS but DO NOT change ACCU.
    let mut rec = MotorRecord::new();
    // The accel cross-calcs are C special() — runtime semantics, so init first
    // (a load leaves ACCL/ACCS raw; see motor_sync_speed_at_init).
    rec.init_record(0).unwrap();
    rec.init_record(1).unwrap();
    rec.vel.velo = 10.0;
    rec.vel.vbas = 0.0;
    rec.vel.accu = AccsUsed::Accl; // default master — must survive the put
    rec.put_field("ACCS", EpicsValue::Double(4.0)).unwrap();
    assert_eq!(rec.vel.accu, AccsUsed::Accl);
    assert!((rec.vel.accs - 4.0).abs() < 1e-12);
    // ACCL = (10 - 0) / 4 = 2.5
    assert!((rec.vel.accl - 2.5).abs() < 1e-12);
}

#[test]
fn test_put_accs_nonpositive_derives_from_accl_not_literal() {
    // C special() motorRecordACCS (motorRecord.cc:2745-2748): a non-positive
    // ACCS is derived from ACCL via updateACCSfromACCL (accs = velo/accl), NOT
    // forced to a literal 1.0.
    let mut rec = MotorRecord::new();
    // The accel cross-calcs are C special() — runtime semantics, so init first
    // (a load leaves ACCL/ACCS raw; see motor_sync_speed_at_init).
    rec.init_record(0).unwrap();
    rec.init_record(1).unwrap();
    rec.vel.velo = 10.0;
    rec.vel.vbas = 0.0;
    rec.vel.accl = 2.0;
    rec.put_field("ACCS", EpicsValue::Double(0.0)).unwrap();
    // accs <= 0 → accs = (10 - 0) / 2 = 5 (from ACCL), not 1.0
    assert!((rec.vel.accs - 5.0).abs() < 1e-12, "ACCS={}", rec.vel.accs);
    // ACCL stays consistent: (10 - 0) / 5 = 2 (round-trip)
    assert!((rec.vel.accl - 2.0).abs() < 1e-12, "ACCL={}", rec.vel.accl);
}

#[test]
fn test_put_velo_cascades_to_accs_when_accu_is_accl() {
    let mut rec = MotorRecord::new();
    // The accel cross-calcs are C special() — runtime semantics, so init first
    // (a load leaves ACCL/ACCS raw; see motor_sync_speed_at_init).
    rec.init_record(0).unwrap();
    rec.init_record(1).unwrap();
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
    // The accel cross-calcs are C special() — runtime semantics, so init first
    // (a load leaves ACCL/ACCS raw; see motor_sync_speed_at_init).
    rec.init_record(0).unwrap();
    rec.init_record(1).unwrap();
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
    // The accel cross-calcs are C special() — runtime semantics, so init first
    // (a load leaves ACCL/ACCS raw; see motor_sync_speed_at_init).
    rec.init_record(0).unwrap();
    rec.init_record(1).unwrap();
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
    // C special RDBD (2764-2766): a runtime RDBD put is clamped to
    // >= |MRES| on the spot. Runtime semantics — init first.
    let mut rec = MotorRecord::new();
    rec.conv.mres = 0.5;
    rec.init_record(0).unwrap();
    rec.init_record(1).unwrap();
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
fn test_rdbd_load_order_independent_at_init() {
    // C: dbLoadRecords writes RDBD raw and the single enforcement runs
    // at init against the final MRES (642, after
    // check_speed_and_resolution). Regression: the load-time RDBD put
    // used to clamp against the default MRES of 1.0, so field(RDBD,0.05)
    // before field(MRES,0.01) booted with RDBD = 1.0.
    for rdbd_first in [true, false] {
        let mut rec = MotorRecord::new();
        if rdbd_first {
            rec.put_field("RDBD", EpicsValue::Double(0.05)).unwrap();
            rec.put_field("MRES", EpicsValue::Double(0.01)).unwrap();
        } else {
            rec.put_field("MRES", EpicsValue::Double(0.01)).unwrap();
            rec.put_field("RDBD", EpicsValue::Double(0.05)).unwrap();
        }
        rec.init_record(0).unwrap();
        rec.init_record(1).unwrap();

        assert_eq!(
            rec.retry.rdbd, 0.05,
            "rdbd_first={rdbd_first}: loaded RDBD above |MRES| must survive init"
        );
    }
}

#[test]
fn test_rdbd_below_mres_raised_at_init() {
    // C 642: a loaded RDBD below |MRES| is raised to |MRES| at init.
    let mut rec = MotorRecord::new();
    rec.put_field("RDBD", EpicsValue::Double(0.005)).unwrap();
    rec.put_field("MRES", EpicsValue::Double(0.01)).unwrap();
    rec.init_record(0).unwrap();
    rec.init_record(1).unwrap();

    assert_eq!(rec.retry.rdbd, 0.01);
}

#[test]
fn test_bdst_put_enforces_min_retry_deadband() {
    // C special BDST (2986-2989): a runtime BDST put re-runs
    // enforceMinRetryDeadband.
    let mut rec = MotorRecord::new();
    rec.conv.mres = 0.5;
    rec.init_record(0).unwrap();
    rec.init_record(1).unwrap();
    rec.retry.rdbd = 0.1; // direct write below |MRES|
    rec.put_field("BDST", EpicsValue::Double(1.0)).unwrap();
    assert_eq!(rec.retry.rdbd, 0.5);
}

#[test]
fn test_mres_change_enforces_min_retry_deadband() {
    // A runtime MRES change restores RDBD >= |MRES| (C does it at the
    // next do_work pass, 1971; the Rust cascade enforces immediately).
    // Runtime semantics — during load the put lands raw and init owns
    // the reconcile.
    let mut rec = MotorRecord::new();
    rec.conv.mres = 0.001;
    rec.init_record(0).unwrap();
    rec.init_record(1).unwrap();
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
    assert_eq!(rec.stat.phase, MotionPhase::MainMove);
    assert!(
        rec.stat.mip.contains(MipFlags::RETRY),
        "retry marked in MIP"
    );
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
    rec.put_field("ACCS", EpicsValue::Double(7.0)).unwrap();
    rec.put_field("ACCU", EpicsValue::Short(1)).unwrap(); // select ACCS as master
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
    rec.put_field("ACCS", EpicsValue::Double(4.0)).unwrap();
    rec.put_field("VELO", EpicsValue::Double(99.0)).unwrap();
    rec.put_field("VBAS", EpicsValue::Double(50.0)).unwrap();
    // select ACCS as master (ACCS/VELO/VBAS puts do not change ACCU themselves)
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
    // The accel cross-calcs are C special() — runtime semantics, so init first
    // (a load leaves ACCL/ACCS raw; see motor_sync_speed_at_init).
    rec.init_record(0).unwrap();
    rec.init_record(1).unwrap();
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
    sim.move_to_home(&user, 3.0, 0.0, 1.0, 0.5).unwrap();
    let status = sim.poll(&user).unwrap();
    // The sim must now be moving toward 3.0 from its start at 0.0.
    assert!(status.moving, "move_to_home should start a move");
}

// Position-compare output (PCO) tests were removed with the framework PCO
// surface: the C base-class PCO change (05b25c1d, motor PR #248) is an open,
// unmerged PR, so PCO is driver-private (see asyn-rs interfaces/motor.rs).

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
    // C devMotorAsyn: rvel = floor(raw velocity); mres=1 → floor(3.7)
    assert_eq!(rec.get_field("RVEL"), Some(EpicsValue::Int64(3)));
}

#[test]
fn test_rvel_converts_egu_velocity_to_raw_steps() {
    use asyn_rs::interfaces::motor::MotorStatus;
    // C devMotorAsyn.c:469-474 fills RVEL with the raw steps/s the
    // controller reports. The Rust driver reports EGU/s, so the record
    // converts through MRES like it does for RMP.
    let mut rec = MotorRecord::new();
    rec.conv.mres = 0.001;
    let status = MotorStatus {
        velocity: 2.5, // EGU/s
        ..Default::default()
    };
    rec.process_motor_info(&status);
    assert_eq!(
        rec.get_field("RVEL"),
        Some(EpicsValue::Int64(2500)),
        "2.5 EGU/s at 0.001 EGU/step = 2500 steps/s"
    );

    // Negative raw velocity keeps C's floor() semantics.
    let status = MotorStatus {
        velocity: -0.0015,
        ..Default::default()
    };
    rec.process_motor_info(&status);
    assert_eq!(
        rec.get_field("RVEL"),
        Some(EpicsValue::Int64(-2)),
        "floor(-1.5) = -2, matching C floor()"
    );
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
    // The accel cross-calcs are C special() — runtime semantics, so init first
    // (a load leaves ACCL/ACCS raw; see motor_sync_speed_at_init).
    rec.init_record(0).unwrap();
    rec.init_record(1).unwrap();
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
fn test_accu_cascade_recomputes_slave_with_full_velo_when_velo_equals_vbas() {
    // C updateACCL_ACCSfromVELO (motorRecord.cc:523-546): at velo <= vbas the
    // numerator is the full velo (not the span), and the slave is STILL
    // recomputed/posted — it is not left at its stale value.
    let mut rec = MotorRecord::new();
    // The accel cross-calcs are C special() — runtime semantics, so init first
    // (a load leaves ACCL/ACCS raw; see motor_sync_speed_at_init).
    rec.init_record(0).unwrap();
    rec.init_record(1).unwrap();
    rec.vel.accu = AccsUsed::Accl;
    rec.vel.accl = 2.0;
    rec.vel.accs = 5.0; // pre-existing slave value (must be overwritten)
    rec.vel.vbas = 5.0;
    rec.put_field("VELO", EpicsValue::Double(5.0)).unwrap();
    // velo == vbas → numerator = velo = 5.0; ACCS = 5.0 / 2.0 = 2.5
    assert!((rec.vel.accs - 2.5).abs() < 1e-12, "ACCS={}", rec.vel.accs);
}

#[test]
fn test_put_accl_recomputes_accs_with_full_velo_when_velo_le_vbas() {
    // C updateACCSfromACCL (motorRecord.cc:512-521): writing ACCL recomputes
    // ACCS; at velo <= vbas the numerator is the full velo, not the span.
    let mut rec = MotorRecord::new();
    // The accel cross-calcs are C special() — runtime semantics, so init first
    // (a load leaves ACCL/ACCS raw; see motor_sync_speed_at_init).
    rec.init_record(0).unwrap();
    rec.init_record(1).unwrap();
    // velo < vbas as record STATE (C reaches it by struct write or a VBAS
    // raise; a runtime VELO put cannot — special() range_checks VELO into
    // [VBAS, VMAX], motorRecord.cc:2690).
    rec.vel.vbas = 8.0;
    rec.vel.velo = 5.0;
    rec.vel.accs = 99.0; // stale slave that must be overwritten
    rec.put_field("ACCL", EpicsValue::Double(2.0)).unwrap();
    assert_eq!(rec.vel.accu, AccsUsed::Accl);
    // velo < vbas → numerator = velo = 5.0; ACCS = 5.0 / 2.0 = 2.5
    assert!((rec.vel.accs - 2.5).abs() < 1e-12, "ACCS={}", rec.vel.accs);
}

#[test]
fn test_put_accs_recomputes_accl_with_full_velo_when_velo_le_vbas() {
    // C updateACCLfromACCS (motorRecord.cc:499-510): writing ACCS recomputes
    // ACCL; at velo <= vbas the numerator is the full velo, not the span.
    let mut rec = MotorRecord::new();
    // The accel cross-calcs are C special() — runtime semantics, so init first
    // (a load leaves ACCL/ACCS raw; see motor_sync_speed_at_init).
    rec.init_record(0).unwrap();
    rec.init_record(1).unwrap();
    // velo < vbas as record STATE (see the ACCL twin above).
    rec.vel.vbas = 8.0;
    rec.vel.velo = 5.0;
    rec.vel.accl = 99.0; // stale slave that must be overwritten
    rec.put_field("ACCS", EpicsValue::Double(4.0)).unwrap();
    // ACCS put no longer switches ACCU (R22 / 63bfe5d0); default master survives.
    assert_eq!(rec.vel.accu, AccsUsed::Accl);
    // velo < vbas → numerator = velo = 5.0; ACCL = 5.0 / 4.0 = 1.25
    assert!((rec.vel.accl - 1.25).abs() < 1e-12, "ACCL={}", rec.vel.accl);
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
    // C 2087: refuse when val > hlm - jvel — one JVEL of decel margin.
    let mut rec = MotorRecord::new();
    rec.put_field("HLM", EpicsValue::Double(10.0)).unwrap();
    rec.put_field("LLM", EpicsValue::Double(-10.0)).unwrap();
    rec.vel.jvel = 1.0;
    rec.pos.val = 9.5; // inside the limit but within the JVEL window
    rec.pos.dval = 9.5;
    rec.ctrl.jogf = true;
    let effects = rec.plan_motion(CommandSource::Jogf);
    assert!(
        !effects
            .commands
            .iter()
            .any(|c| matches!(c, MotorCommand::MoveVelocity { .. })),
        "forward jog inside the JVEL window should not emit MoveVelocity"
    );
    assert!(!rec.ctrl.jogf, "JOGF button cleared");
    assert!(rec.limits.lvio);
    assert_eq!(rec.stat.phase, MotionPhase::Idle);
}

#[test]
fn test_reverse_jog_at_low_limit_is_rejected_and_clears_button() {
    // C 2088: refuse when val < llm + jvel.
    let mut rec = MotorRecord::new();
    rec.put_field("HLM", EpicsValue::Double(10.0)).unwrap();
    rec.put_field("LLM", EpicsValue::Double(-10.0)).unwrap();
    rec.vel.jvel = 1.0;
    rec.pos.val = -9.5;
    rec.pos.dval = -9.5;
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
    rec.vel.jvel = 1.0;
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

    // Driver reports done → queued jog resumes. The completion gate is
    // MOVN (C 1301/1345), which the poll recomputes from the status.
    rec.stat.msta = MstaFlags::DONE;
    rec.stat.movn = false;
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

#[test]
fn test_urip_skipped_at_init_readback_applied_on_poll() {
    use asyn_rs::interfaces::motor::MotorStatus;
    // R41 / C process_motor_info(pmr, initcall): the URIP RDBL scaling is
    // gated `else if (urip==Yes && initcall==false)` (motorRecord.cc:3682),
    // so the init readback seeds RRBV/DRBV from the MOTOR position, not the
    // external readback link. Subsequent runtime polls (initcall=false) DO
    // apply the link. An earlier Rust gate on `self.initialized` never
    // skipped the init readback because `determine_event` flips it true
    // before dispatching Startup.
    let status = MotorStatus {
        position: 3.0, // motor dial position
        done: true,
        moving: false,
        ..Default::default()
    };

    // Init readback: URIP suppressed, DRBV adopts the motor position (3.0).
    let mut rec = MotorRecord::new();
    rec.conv.mres = 1.0;
    rec.conv.urip = true;
    rec.conv.rres = 1.0;
    rec.conv.rdbl_value = Some(7.0); // external link disagrees with the motor
    rec.initial_readback(&status);
    assert_eq!(rec.pos.rrbv, 3, "init readback adopts the motor position");
    assert_eq!(rec.pos.drbv, 3.0, "init DRBV from motor position, not RDBL");

    // A runtime poll on the same record DOES apply the URIP link (7.0).
    rec.process_motor_info(&status);
    assert_eq!(rec.pos.rrbv, 7, "runtime poll applies the URIP RDBL link");
    assert_eq!(rec.pos.drbv, 7.0, "runtime DRBV from the external link");
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
fn test_urip_rdbl_error_refuses_in_flight_retarget_without_stop() {
    // C move-block gate (2418-2453): a target write with the RDBL link
    // down is refused and rolled back to the lasts — the put pass
    // itself sends nothing. Halting the axis is owned by the poll-time
    // read failure (3687-3697), exercised below and in the
    // _on_completion_check test.
    let mut rec = MotorRecord::new();
    rec.conv.urip = true;
    rec.set_rdbl_error(false); // start healthy
    rec.put_field("VAL", EpicsValue::Double(10.0)).unwrap();
    rec.plan_motion(CommandSource::Val);
    assert_eq!(rec.stat.phase, MotionPhase::MainMove);

    // RDBL link errors; a new target arrives.
    rec.set_rdbl_error(true);
    rec.put_field("VAL", EpicsValue::Double(20.0)).unwrap();
    let effects = rec.plan_motion(CommandSource::Val);
    assert!(effects.commands.is_empty(), "refusal sends no command");
    assert_eq!(
        rec.pos.val, 10.0,
        "refused target rolls back to the in-flight target (C 2436)"
    );
    assert_eq!(rec.stat.phase, MotionPhase::MainMove);

    // The next poll pass stops the axis (C 3691-3697).
    let effects = rec.check_completion();
    assert!(
        effects
            .commands
            .iter()
            .any(|c| matches!(c, MotorCommand::Stop { .. }))
    );
}

#[test]
fn test_urip_rdbl_error_jog_and_home_dispatch_normally() {
    // C 7493d50b changelog: the refusal is "sans Home search or Jog" —
    // the home (2010) and jog (2078) sections run before the
    // move-block rtnstat gate (2418-2453), so both dispatch with the
    // readback link down.
    let mut rec = MotorRecord::new();
    rec.conv.urip = true;
    rec.set_rdbl_error(true);
    rec.stat.msta = MstaFlags::DONE;
    rec.put_field("JOGF", EpicsValue::Short(1)).unwrap();
    let effects = rec.plan_motion(CommandSource::Jogf);
    assert!(
        effects
            .commands
            .iter()
            .any(|c| matches!(c, MotorCommand::MoveVelocity { .. })),
        "jog dispatches with the RDBL link down"
    );

    let mut rec = MotorRecord::new();
    rec.conv.urip = true;
    rec.set_rdbl_error(true);
    rec.stat.msta = MstaFlags::DONE;
    rec.put_field("HOMF", EpicsValue::Short(1)).unwrap();
    let effects = rec.plan_motion(CommandSource::Homf);
    assert!(
        effects
            .commands
            .iter()
            .any(|c| matches!(c, MotorCommand::Home { .. })),
        "home dispatches with the RDBL link down"
    );
}

#[test]
fn test_urip_rdbl_error_set_mode_write_escapes_refusal() {
    // C SET branches (2207, 2257-2262) return before the rtnstat gate:
    // a SET-mode redefinition proceeds with the RDBL link down and is
    // not rolled back.
    let mut rec = MotorRecord::new();
    rec.conv.urip = true;
    rec.set_rdbl_error(true);
    rec.conv.set = true;
    rec.stat.msta = MstaFlags::DONE;
    rec.put_field("DVAL", EpicsValue::Double(5.0)).unwrap();
    rec.plan_motion(CommandSource::Dval);
    assert_eq!(rec.pos.dval, 5.0, "SET-mode write not rolled back");
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

#[test]
fn test_urip_rdbl_error_stop_clears_latched_buttons() {
    // C 3692-3694: the failed-RDBL stop runs clear_buttons() before
    // setting stop=1, releasing any held jog/home button so the halted
    // axis is not re-commanded on the next pass.
    let mut rec = MotorRecord::new();
    rec.conv.urip = true;
    rec.ctrl.jogf = true;
    rec.plan_motion(CommandSource::Jogf);
    assert_ne!(rec.stat.phase, MotionPhase::Idle);

    rec.set_rdbl_error(true);
    let effects = rec.check_completion();
    assert!(
        effects
            .commands
            .iter()
            .any(|c| matches!(c, MotorCommand::Stop { .. }))
    );
    assert!(!rec.ctrl.jogf, "JOGF released by the error stop");
}

#[test]
fn test_rdbl_resolution_report_drives_rdbl_error() {
    // The framework reports which requested link reads resolved
    // (set_resolved_input_links) — the analogue of C inspecting
    // RTN_SUCCESS(dbGetLink(&pmr->rdbl,...)) at motorRecord.cc:3687.
    use epics_base_rs::server::record::Record;
    let mut rec = MotorRecord::new();
    rec.conv.urip = true;
    rec.links.rdbl = "ext_readback.RBV".to_string();

    rec.set_resolved_input_links(&[]);
    assert!(
        rec.conv.rdbl_error,
        "missing RDBL in the report = failed read"
    );

    rec.set_resolved_input_links(&["RDBL"]);
    assert!(!rec.conv.rdbl_error, "resolved RDBL clears the error");

    // UEIP=Yes wins the C else-if chain (3676): the RDBL read is not
    // requested, so a stale report must not re-latch the error.
    rec.conv.ueip = true;
    rec.set_resolved_input_links(&[]);
    assert!(!rec.conv.rdbl_error, "no RDBL read under UEIP=Yes");
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
        encoder_home: false,
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
        encoder_home: false,
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
        encoder_home: false,
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
        encoder_home: false,
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
    rec.conv.mres = 0.01;
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
    // C syncTargetPosition (4455): rval = NINT(dval / mres)
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

// --- Closed-loop OMSL collection gate (C do_work 1994-2198, postProcess 827) ---

#[test]
fn test_closed_loop_db_dol_suppresses_button_dispatch() {
    // C 1994-2008: with OMSL=closed_loop and a DB-link DOL, the whole
    // button/jog/tweak collection else (2008-2198) is bypassed — a
    // latched JOGF/HOMF never dispatches, and the button keeps its
    // value (C writes the field; the collection never reads it), so an
    // OMSL flip back to supervisory can still act on it.
    let mut rec = MotorRecord::new();
    rec.links.omsl = 1;
    rec.links.dol = "setpoint_src.VAL".to_string();
    rec.stat.msta = MstaFlags::DONE;

    rec.put_field("JOGF", EpicsValue::Short(1)).unwrap();
    let effects = rec.plan_motion(CommandSource::Jogf);
    assert!(effects.commands.is_empty(), "no jog under closed loop");
    assert_eq!(rec.stat.phase, MotionPhase::Idle);
    assert!(rec.ctrl.jogf, "button stays latched for an OMSL flip");

    rec.ctrl.jogf = false;
    rec.put_field("HOMF", EpicsValue::Short(1)).unwrap();
    let effects = rec.plan_motion(CommandSource::Homf);
    assert!(effects.commands.is_empty(), "no home under closed loop");
    assert_eq!(rec.stat.phase, MotionPhase::Idle);
    assert!(rec.ctrl.homf, "button stays latched");
}

#[test]
fn test_closed_loop_constant_dol_keeps_buttons_active() {
    // C 1994 requires dol.type == DB_LINK: a constant DOL under closed
    // loop leaves the collection block running.
    let mut rec = MotorRecord::new();
    rec.links.omsl = 1;
    rec.links.dol = "5.0".to_string(); // CONSTANT, not a DB link
    rec.stat.msta = MstaFlags::DONE;

    rec.put_field("JOGF", EpicsValue::Short(1)).unwrap();
    let effects = rec.plan_motion(CommandSource::Jogf);
    assert!(
        !effects.commands.is_empty(),
        "constant DOL: jog still dispatches"
    );
}

#[test]
fn test_closed_loop_tweak_and_rlv_not_collected() {
    // C 2167-2197: the tweak fold and the RLV fold sit inside the
    // bypassed else — the writes land in the fields and stay inert.
    let mut rec = MotorRecord::new();
    rec.links.omsl = 1;
    rec.links.dol = "setpoint_src.VAL".to_string();
    rec.stat.msta = MstaFlags::DONE;
    rec.pos.val = 10.0;
    rec.pos.dval = 10.0;
    rec.ctrl.twv = 1.0;

    rec.put_field("TWF", EpicsValue::Short(1)).unwrap();
    let effects = rec.plan_motion(CommandSource::Twf);
    assert!(effects.commands.is_empty(), "no tweak move");
    assert_eq!(rec.pos.val, 10.0, "no tweak fold under closed loop");
    assert!(rec.ctrl.twf, "TWF stays latched");

    rec.put_field("RLV", EpicsValue::Double(2.0)).unwrap();
    let effects = rec.plan_motion(CommandSource::Rlv);
    assert!(effects.commands.is_empty(), "no relative move");
    assert_eq!(rec.pos.val, 10.0, "no RLV fold");
    assert_eq!(rec.pos.rlv, 2.0, "RLV stays latched");
}

#[test]
fn test_closed_loop_rval_put_is_raw_struct_write() {
    // C special() has no RVAL branch; the RVAL->DVAL propagation
    // (2196-2197) is inside the bypassed else — a closed-loop RVAL put
    // lands in the field and stays inert, in move and SET mode alike.
    let mut rec = MotorRecord::new();
    rec.links.omsl = 1;
    rec.links.dol = "setpoint_src.VAL".to_string();
    rec.conv.mres = 0.01;
    rec.pos.val = 10.0;
    rec.pos.dval = 10.0;
    rec.stat.msta = MstaFlags::DONE;

    rec.put_field("RVAL", EpicsValue::Int64(5000)).unwrap();
    assert_eq!(rec.pos.rval, 5000, "field takes the raw write");
    assert_eq!(rec.pos.dval, 10.0, "no RVAL->DVAL propagation");
    assert_eq!(rec.pos.val, 10.0);
    let effects = rec.plan_motion(CommandSource::Rval);
    assert!(effects.commands.is_empty(), "inert under closed loop");

    rec.conv.set = true;
    rec.put_field("RVAL", EpicsValue::Int64(7000)).unwrap();
    assert_eq!(rec.pos.rval, 7000);
    assert_eq!(rec.pos.dval, 10.0, "no SET redefinition under closed loop");
}

#[test]
fn test_closed_loop_latent_tweak_skipped_on_val_pass() {
    // A latched TWF must not ride a closed-loop VAL pass: the fold
    // (2167) is inside the bypassed else even when the pass itself is
    // the DOL cascade arriving through VAL.
    let mut rec = MotorRecord::new();
    rec.links.omsl = 1;
    rec.links.dol = "setpoint_src.VAL".to_string();
    rec.stat.msta = MstaFlags::DONE;
    rec.ctrl.twv = 1.0;
    rec.ctrl.twf = true; // latched before the DOL pass

    rec.put_field("VAL", EpicsValue::Double(20.0)).unwrap();
    let effects = rec.plan_motion(CommandSource::Val);
    assert!(!effects.commands.is_empty(), "DOL cascade still moves");
    assert_eq!(rec.pos.val, 20.0, "no +TWV fold");
    assert!(rec.ctrl.twf, "button stays latched");
}

// --- Level-triggered latent collection (C do_work gate, 1486-1492) ---

#[test]
fn test_latent_jog_fires_on_no_event_pass_after_omsl_flip() {
    // C do_work runs on every put pass (1486-1492) and its jog section
    // (2079) acts on latched button STATE: a JOGF latched under the
    // closed-loop bypass dispatches on the first supervisory pass —
    // here the no-event process pass after the OMSL flip — without
    // JOGF being re-written.
    let mut rec = MotorRecord::new();
    rec.links.omsl = 1;
    rec.links.dol = "setpoint_src.VAL".to_string();
    rec.stat.msta = MstaFlags::DONE;

    rec.put_field("JOGF", EpicsValue::Short(1)).unwrap();
    rec.do_process();
    assert_eq!(
        rec.stat.phase,
        MotionPhase::Idle,
        "bypassed under closed loop"
    );
    assert!(rec.ctrl.jogf, "button latched");

    rec.put_field("OMSL", EpicsValue::Short(0)).unwrap();
    rec.do_process();
    assert_eq!(rec.stat.phase, MotionPhase::Jog, "latched jog dispatches");
    assert!(rec.stat.mip.contains(MipFlags::JOGF));
    assert!(!rec.stat.dmov);
}

#[test]
fn test_latent_tweak_folds_and_moves_after_omsl_flip() {
    // C tweak section (2167-2181): a TWF latched under the closed-loop
    // bypass folds into VAL on the first supervisory do_work pass and
    // the move block dispatches the combined move.
    let mut rec = MotorRecord::new();
    rec.links.omsl = 1;
    rec.links.dol = "setpoint_src.VAL".to_string();
    rec.conv.mres = 0.01;
    rec.stat.msta = MstaFlags::DONE;
    rec.pos.val = 10.0;
    rec.pos.dval = 10.0;
    rec.ctrl.twv = 1.0;

    rec.put_field("TWF", EpicsValue::Short(1)).unwrap();
    rec.do_process();
    assert_eq!(rec.pos.val, 10.0, "no fold under closed loop");
    assert!(rec.ctrl.twf, "TWF latched");

    rec.put_field("OMSL", EpicsValue::Short(0)).unwrap();
    rec.do_process();
    assert_eq!(rec.pos.val, 11.0, "fold lands on the supervisory pass");
    assert!(!rec.ctrl.twf, "button consumed by the fold");
    assert_eq!(rec.stat.phase, MotionPhase::MainMove, "move dispatched");
}

#[test]
fn test_limit_blocked_homf_fires_on_idle_poll_when_limit_clears() {
    use asyn_rs::interfaces::motor::MotorStatus;
    // C home-section gate (2013-2014): a HOMF blocked by the high-limit
    // switch stays latched; the section re-evaluates the STATE on every
    // do_work pass, so the done-record callback reporting the switch
    // released (the dmov arm of the 1486-1492 gate) dispatches the home.
    let mut rec = MotorRecord::new();
    rec.conv.mres = 0.01;
    rec.limits.hls = true;
    rec.stat.msta = MstaFlags::DONE;

    rec.put_field("HOMF", EpicsValue::Short(1)).unwrap();
    rec.do_process();
    assert_eq!(rec.stat.phase, MotionPhase::Idle, "blocked by HLS");
    assert!(rec.ctrl.homf, "button stays latched");

    // The poll that reports the switch released.
    let cleared = MotorStatus {
        done: true,
        ..Default::default()
    };
    rec.set_event(MotorEvent::DeviceUpdate(cleared));
    rec.do_process();
    assert_eq!(
        rec.stat.phase,
        MotionPhase::Homing,
        "latched home dispatches"
    );
    assert!(rec.stat.mip.contains(MipFlags::HOMF));
    assert!(rec.ctrl.homf, "button reads back 1 for the whole home");
}

#[test]
fn test_stup_ack_pass_runs_latent_collection() {
    use asyn_rs::interfaces::motor::MotorStatus;
    // C 1345 + 1486-1492: the GET_INFO ack callback skips the
    // motor-stopped branch but still falls into do_work via the dmov
    // arm, so a home latched behind a limit switch dispatches on the
    // ack pass that reports the switch released.
    let mut rec = MotorRecord::new();
    rec.conv.mres = 0.01;
    rec.limits.hls = true;
    rec.stat.msta = MstaFlags::DONE;

    rec.put_field("HOMF", EpicsValue::Short(1)).unwrap();
    rec.do_process();
    assert!(rec.ctrl.homf, "blocked HOMF stays latched");
    assert_eq!(rec.stat.phase, MotionPhase::Idle);

    let cleared = MotorStatus {
        done: true,
        ..Default::default()
    };
    rec.internal.stup_ack = true;
    rec.set_event(MotorEvent::DeviceUpdate(cleared));
    rec.do_process();
    assert_eq!(
        rec.stat.phase,
        MotionPhase::Homing,
        "ack pass dispatches the latched home"
    );
    assert!(rec.stat.mip.contains(MipFlags::HOMF));
}

#[test]
fn test_stop_sync_skipped_under_closed_loop_omsl() {
    // C postProcess 827: the VAL/DVAL/RVAL <- readback resync at stop
    // completion is gated on omsl != closed_loop ALONE (no dol.type
    // test) — the drive values belong to the DOL cascade.
    let mut rec = MotorRecord::new();
    rec.links.omsl = 1; // no DB-link DOL needed: the C gate tests OMSL only
    rec.stat.phase = MotionPhase::MainMove;
    rec.stat.mip = MipFlags::MOVE;
    rec.stat.dmov = false;
    rec.pos.val = 50.0;
    rec.pos.dval = 50.0;
    rec.pos.rbv = 25.0;
    rec.pos.drbv = 25.0;

    rec.ctrl.spmg = SpmgMode::Stop;
    let effects = rec.plan_motion(CommandSource::Spmg);
    assert!(matches!(effects.commands[0], MotorCommand::Stop { .. }));

    rec.stat.msta = MstaFlags::DONE;
    rec.stat.movn = false;
    rec.pos.rbv = 30.0;
    rec.pos.drbv = 30.0;
    let _ = rec.check_completion();
    assert!(rec.stat.dmov);
    assert_eq!(rec.stat.phase, MotionPhase::Idle);
    assert_eq!(rec.pos.val, 50.0, "closed loop keeps the DOL target");
    assert_eq!(rec.pos.dval, 50.0);
}

#[test]
fn test_sync_pv_resyncs_under_closed_loop() {
    // C syncTargetPosition (4445-4460) has NO OMSL gate — unlike the
    // postProcess resync, a SYNC=1 put reseeds the drive triplet even
    // under closed loop.
    let mut rec = MotorRecord::new();
    rec.links.omsl = 1;
    rec.links.dol = "setpoint_src.VAL".to_string();
    rec.conv.mres = 0.01;
    rec.pos.drbv = 25.0;
    rec.pos.rbv = 25.0;
    rec.pos.rrbv = 2500;
    rec.pos.val = 0.0;
    rec.pos.dval = 0.0;

    rec.put_field("SYNC", EpicsValue::Short(1)).unwrap();
    rec.plan_motion(CommandSource::Sync);
    assert_eq!(rec.pos.val, 25.0);
    assert_eq!(rec.pos.dval, 25.0);
    assert_eq!(rec.pos.rval, 2500);
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
        has_encoder: true,
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
    // FOFF=Frozen: the VAL redefinition propagates to DVAL and needs
    // the controller LOAD_POS (C 3811) — refused under the block.
    let mut rec = MotorRecord::new();
    rec.conv.mres = 0.01;
    rec.conv.set = true;
    rec.conv.foff = FreezeOffset::Frozen;
    rec.conv.loadpos_blocked = true;
    rec.pos.dval = 5.0;
    rec.pos.off = 0.0;

    // SET-mode VAL write would normally redefine DVAL — must be refused.
    rec.put_field("VAL", EpicsValue::Double(100.0)).unwrap();
    assert_eq!(rec.pos.dval, 5.0); // unchanged
    assert_eq!(rec.pos.off, 0.0); // unchanged — DVAL/OFF stay consistent
}

#[test]
fn test_loadpos_block_allows_offset_only_val_redefinition() {
    // FOFF=Variable: C 2206-2227 redefines VAL by adjusting OFF only —
    // no controller command — so the #231 block must not refuse it.
    let mut rec = MotorRecord::new();
    rec.conv.mres = 0.01;
    rec.conv.set = true;
    rec.conv.loadpos_blocked = true;
    rec.pos.dval = 5.0;
    rec.pos.off = 0.0;

    rec.put_field("VAL", EpicsValue::Double(100.0)).unwrap();
    assert_eq!(rec.pos.off, 95.0, "OFF absorbs the redefinition");
    assert_eq!(rec.pos.dval, 5.0, "DVAL untouched (offset-only)");
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
fn test_rhlm_rllm_loaded_raw_wins_at_init() {
    // C dbd: RHLM/RLLM are SPC_NOMOD — a put models the database
    // field() load (raw struct write, no cascade); the raw-wins rule
    // (check_speed_and_resolution 3968-3992) derives the dial pair at
    // init.
    let mut rec = MotorRecord::new();
    rec.conv.mres = 0.001;
    rec.put_field("RHLM", EpicsValue::Double(10_000.0)).unwrap();
    rec.put_field("RLLM", EpicsValue::Double(-10_000.0))
        .unwrap();
    assert_eq!(rec.get_field("RHLM"), Some(EpicsValue::Double(10_000.0)));
    assert_eq!(rec.get_field("RLLM"), Some(EpicsValue::Double(-10_000.0)));
    // No cascade at load time (C: special never runs during dbLoadRecords).
    assert_eq!(rec.limits.dhlm, 0.0);
    rec.init_record(1).unwrap();
    // Raw wins: dial derived as raw * mres.
    assert!((rec.limits.dhlm - 10.0).abs() < 1e-9);
    assert!((rec.limits.dllm + 10.0).abs() < 1e-9);
}

#[test]
fn test_mres_change_preserves_raw_limits() {
    let mut rec = MotorRecord::new();
    rec.conv.mres = 0.01;
    rec.put_field("DHLM", EpicsValue::Double(100.0)).unwrap();
    rec.put_field("DLLM", EpicsValue::Double(-100.0)).unwrap();
    // A load-time dial put leaves the raw pair at 0 (C: raw struct
    // write); init seeds it from the dial values.
    assert_eq!(rec.limits.rhlm, 0.0);
    assert_eq!(rec.limits.rllm, 0.0);

    // init_record establishes the limit invariant — only after this does a
    // runtime MRES change keep RHLM/RLLM invariant and rescale DHLM/DLLM.
    rec.init_record(1).unwrap();
    assert!((rec.limits.rhlm - 10_000.0).abs() < 1e-6);
    assert!((rec.limits.rllm + 10_000.0).abs() < 1e-6);

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

#[test]
fn test_rhlm_rllm_registry_read_only() {
    // C dbd (2e89b552): RHLM/RLLM carry special(SPC_NOMOD) — runtime CA
    // writes are refused at the server layer via FieldDesc.read_only.
    let rec = MotorRecord::new();
    let fields = rec.field_list();
    for name in ["RHLM", "RLLM"] {
        let f = fields
            .iter()
            .find(|f| f.name == name)
            .unwrap_or_else(|| panic!("{name} missing from field list"));
        assert!(f.read_only, "{name} must be SPC_NOMOD (read-only)");
    }
}

#[test]
fn test_init_negative_mres_loaded_rllm_drives_dhlm() {
    // C check_speed_and_resolution (3993-4017): under MRES < 0 a loaded
    // RLLM seeds DHLM through fabs(mres). Then init_record 716-718 runs
    // set_dial_high/lowlimit, whose SIGNED re-derivation lands
    // dhlm/mres in the LOW raw register — flipping the sign of the
    // fabs-seeded raw value (C verbatim quirk).
    let mut rec = MotorRecord::new();
    rec.put_field("MRES", EpicsValue::Double(-0.001)).unwrap();
    rec.put_field("RLLM", EpicsValue::Double(50_000.0)).unwrap();
    rec.init_record(1).unwrap();
    assert!(
        (rec.limits.dhlm - 50.0).abs() < 1e-9,
        "DHLM={}",
        rec.limits.dhlm
    );
    assert_eq!(rec.limits.dllm, 0.0);
    assert!(
        (rec.limits.rllm + 50_000.0).abs() < 1e-6,
        "RLLM={}",
        rec.limits.rllm
    );
    assert!(rec.limits.rhlm.abs() < 1e-6, "RHLM={}", rec.limits.rhlm);
}

#[test]
fn test_runtime_negative_mres_crosses_dial_keeps_raw() {
    // C special MRES (2877-2906): the raw pair is the invariant across a
    // resolution change; MRES < 0 crosses the dial assignment with the
    // SIGNED resolution (dhlm = rllm * mres, dllm = rhlm * mres).
    let mut rec = MotorRecord::new();
    rec.put_field("DHLM", EpicsValue::Double(100.0)).unwrap();
    rec.put_field("DLLM", EpicsValue::Double(-10.0)).unwrap();
    rec.init_record(1).unwrap();
    assert!((rec.limits.rhlm - 100.0).abs() < 1e-9);
    assert!((rec.limits.rllm + 10.0).abs() < 1e-9);

    rec.put_field("MRES", EpicsValue::Double(-1.0)).unwrap();
    assert!(
        (rec.limits.dhlm - 10.0).abs() < 1e-9,
        "DHLM={}",
        rec.limits.dhlm
    );
    assert!(
        (rec.limits.dllm + 100.0).abs() < 1e-9,
        "DLLM={}",
        rec.limits.dllm
    );
    // Raw registers untouched by the cascade.
    assert!(
        (rec.limits.rhlm - 100.0).abs() < 1e-9,
        "RHLM={}",
        rec.limits.rhlm
    );
    assert!(
        (rec.limits.rllm + 10.0).abs() < 1e-9,
        "RLLM={}",
        rec.limits.rllm
    );
}

#[test]
fn test_runtime_dial_high_put_under_negative_mres_lands_in_rllm() {
    // C set_dial_highlimit (4243-4275, fd808eb2): dhlm / mres with the
    // SIGNED resolution addresses the LOW raw register under MRES < 0
    // (SET_LOW_LIMIT). set_user_highlimit (4076-4147) keys the same way
    // (dir_positive ^ (mres < 0)) — with DIR=Pos an HLM put writes the
    // dial HIGH limit and likewise lands in RLLM.
    let mut rec = MotorRecord::new();
    rec.put_field("MRES", EpicsValue::Double(-1.0)).unwrap();
    rec.put_field("DHLM", EpicsValue::Double(10.0)).unwrap();
    rec.init_record(1).unwrap();
    assert!(
        (rec.limits.rllm + 10.0).abs() < 1e-9,
        "RLLM={}",
        rec.limits.rllm
    );

    rec.put_field("DHLM", EpicsValue::Double(20.0)).unwrap();
    assert!((rec.limits.dhlm - 20.0).abs() < 1e-9);
    assert!(
        (rec.limits.rllm + 20.0).abs() < 1e-9,
        "RLLM={}",
        rec.limits.rllm
    );
    assert!(rec.limits.rhlm.abs() < 1e-6, "RHLM={}", rec.limits.rhlm);

    rec.put_field("HLM", EpicsValue::Double(30.0)).unwrap();
    assert!((rec.limits.dhlm - 30.0).abs() < 1e-9);
    assert!(
        (rec.limits.rllm + 30.0).abs() < 1e-9,
        "RLLM={}",
        rec.limits.rllm
    );
}

// --- Per-field metadata (C RSET get_units 3156-3208, get_precision
//     3313-3337, get_graphic_double 3213-3258, get_control_double
//     3263-3308, get_alarm_double 3344-3361) ---

#[test]
fn test_field_metadata_units() {
    let mut rec = MotorRecord::new();
    rec.put_field("EGU", EpicsValue::String("deg".into()))
        .unwrap();
    let units = |f: &str| rec.field_metadata_override(f).unwrap().units;
    for f in ["VELO", "VMAX", "BVEL", "VBAS", "JVEL", "HVEL"] {
        assert_eq!(units(f).unwrap(), "deg/sec", "{f}");
    }
    assert_eq!(units("JAR").unwrap(), "deg/s/s");
    assert_eq!(units("ACCL").unwrap(), "sec");
    assert_eq!(units("BACC").unwrap(), "sec");
    for f in ["S", "SBAS", "SBAK"] {
        assert_eq!(units(f).unwrap(), "rev/sec", "{f}");
    }
    assert_eq!(units("SREV").unwrap(), "steps/rev");
    assert_eq!(units("UREV").unwrap(), "deg/rev");
    // C default branch: bare EGU — the record-level metadata serves it.
    assert!(units("VAL").is_none());
    assert!(units("RBV").is_none());
}

#[test]
fn test_field_metadata_precision() {
    let mut rec = MotorRecord::new();
    rec.put_field("PREC", EpicsValue::Short(4)).unwrap();
    let prec = |f: &str| rec.field_metadata_override(f).unwrap().precision;
    assert_eq!(prec("RRBV"), Some(0));
    assert_eq!(prec("RMP"), Some(0));
    assert_eq!(prec("REP"), Some(0));
    assert_eq!(prec("VERS"), Some(2));
    // Integer DBF types: recGblGetPrec forces 0 (recGbl.c:124-133).
    assert_eq!(prec("DMOV"), Some(0));
    assert_eq!(prec("RCNT"), Some(0));
    // Double field with in-range PREC keeps the record-level value.
    assert_eq!(prec("VAL"), None);
    // Out-of-range PREC clamps to 15 (recGbl.c:135-138).
    rec.put_field("PREC", EpicsValue::Short(99)).unwrap();
    assert_eq!(
        rec.field_metadata_override("VAL").unwrap().precision,
        Some(15)
    );
}

#[test]
fn test_field_metadata_graphic_control_limits() {
    let mut rec = MotorRecord::new();
    rec.put_field("DHLM", EpicsValue::Double(100.0)).unwrap();
    rec.put_field("DLLM", EpicsValue::Double(-50.0)).unwrap();
    rec.put_field("MRES", EpicsValue::Double(0.5)).unwrap();
    rec.init_record(1).unwrap();
    rec.vel.vmax = 20.0;
    rec.vel.vbas = 0.1;

    let lim = |f: &str| rec.field_metadata_override(f).unwrap().disp_limits;
    assert_eq!(lim("VAL"), Some((rec.limits.hlm, rec.limits.llm)));
    assert_eq!(lim("RBV"), lim("VAL"));
    assert_eq!(lim("DVAL"), Some((100.0, -50.0)));
    assert_eq!(lim("DRBV"), Some((100.0, -50.0)));
    // Raw pair: dial / mres with the SIGNED resolution (C 3235-3239).
    assert_eq!(lim("RVAL"), Some((200.0, -100.0)));
    assert_eq!(lim("RRBV"), Some((200.0, -100.0)));
    assert_eq!(lim("VELO"), Some((20.0, 0.1)));
    // Unlisted double field: recGblGetGraphicDouble type range.
    assert_eq!(lim("ACCL"), Some((1e300, -1e300)));
    // Control mirrors graphic — the C switches are identical.
    let ov = rec.field_metadata_override("VELO").unwrap();
    assert_eq!(ov.ctrl_limits, Some((20.0, 0.1)));
    let ov = rec.field_metadata_override("DVAL").unwrap();
    assert_eq!(ov.ctrl_limits, Some((100.0, -50.0)));
}

#[test]
fn test_field_metadata_raw_limits_cross_under_negative_mres() {
    let mut rec = MotorRecord::new();
    rec.limits.dhlm = 100.0;
    rec.limits.dllm = -50.0;
    rec.conv.mres = -0.5;
    // C 3240-3244: upper = dllm/mres, lower = dhlm/mres.
    let ov = rec.field_metadata_override("RVAL").unwrap();
    assert_eq!(ov.disp_limits, Some((100.0, -200.0)));
    assert_eq!(ov.ctrl_limits, Some((100.0, -200.0)));
}

#[test]
fn test_field_metadata_alarm_limits() {
    let mut rec = MotorRecord::new();
    rec.alarm.hihi = 90.0;
    rec.alarm.high = 80.0;
    rec.alarm.low = -80.0;
    rec.alarm.lolo = -90.0;
    let al = |f: &str| {
        rec.field_metadata_override(f)
            .unwrap()
            .alarm_limits
            .unwrap()
    };
    // C 3349-3355: VAL/DVAL serve the limits unconditionally.
    assert_eq!(al("VAL"), (90.0, 80.0, -80.0, -90.0));
    assert_eq!(al("DVAL"), (90.0, 80.0, -80.0, -90.0));
    // Every other field: recGblGetAlarmDouble NaNs (recGbl.c:155-162).
    let (hihi, high, low, lolo) = al("RBV");
    assert!(hihi.is_nan() && high.is_nan() && low.is_nan() && lolo.is_nan());
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

// --- ACCS/ACCL at init (C check_speed_and_resolution, motorRecord.cc:4033-4047)
//
// C keys the accel reconcile on the LOADED ACCS (`accs > 0.0`), not on ACCU.
// The boundaries are: ACCS loaded nonzero / ACCS zero / ACCL also zero, times
// the load order that decides what VELO is when the pair is reconciled.

#[test]
fn test_load_accs_wins_over_accl_at_init() {
    // C:4034 — a nonzero loaded ACCS is the master and ACCL is derived from it
    // (updateACCLfromACCS: accl = (velo - vbas) / accs = 10 / 5 = 2), whatever
    // ACCU says. VELO is loaded *after* ACCS, so a port that cross-calcs during
    // the load reconciles against the pre-VELO value and loses ACCS.
    let mut rec = MotorRecord::new();
    rec.put_field("ACCS", EpicsValue::Double(5.0)).unwrap();
    rec.put_field("VELO", EpicsValue::Double(10.0)).unwrap();
    rec.init_record(0).unwrap();
    rec.init_record(1).unwrap();

    assert!((rec.vel.accs - 5.0).abs() < 1e-9, "ACCS={}", rec.vel.accs);
    assert!((rec.vel.accl - 2.0).abs() < 1e-9, "ACCL={}", rec.vel.accl);
}

#[test]
fn test_load_accu_accs_without_accs_derives_accs_from_accl_at_init() {
    // C:4038-4047 — ACCU is NOT the init key (63bfe5d0 made it a user/autosave
    // control). With no `field(ACCS,…)` the loaded ACCS is 0, so ACCL is the
    // master even when ACCU says Accs: accs = velo / accl = 10 / 0.2 = 50.
    let mut rec = MotorRecord::new();
    rec.put_field("ACCU", EpicsValue::Short(1)).unwrap(); // AccsUsed::Accs
    rec.put_field("VELO", EpicsValue::Double(10.0)).unwrap();
    rec.init_record(0).unwrap();
    rec.init_record(1).unwrap();

    assert!((rec.vel.accl - 0.2).abs() < 1e-9, "ACCL={}", rec.vel.accl);
    assert!((rec.vel.accs - 50.0).abs() < 1e-9, "ACCS={}", rec.vel.accs);
}

#[test]
fn test_load_zero_accl_and_accs_floors_accl_at_init() {
    // C:4041-4045 — with ACCS zero and ACCL loaded as 0, ACCL is floored to 0.1
    // before ACCS is derived: accs = 10 / 0.1 = 100.
    let mut rec = MotorRecord::new();
    rec.put_field("VELO", EpicsValue::Double(10.0)).unwrap();
    rec.put_field("ACCL", EpicsValue::Double(0.0)).unwrap();
    rec.init_record(0).unwrap();
    rec.init_record(1).unwrap();

    assert!((rec.vel.accl - 0.1).abs() < 1e-9, "ACCL={}", rec.vel.accl);
    assert!((rec.vel.accs - 100.0).abs() < 1e-9, "ACCS={}", rec.vel.accs);
}

#[test]
fn test_runtime_accl_accs_puts_cross_calc_after_init() {
    // The load gate must not disarm C's special() cross-calc: after init an
    // ACCL put re-derives ACCS (motorRecord.cc:2735-2742) and an ACCS put
    // re-derives ACCL (:2745-2752), with the <= 0 floors intact.
    let mut rec = MotorRecord::new();
    rec.put_field("VELO", EpicsValue::Double(10.0)).unwrap();
    rec.init_record(0).unwrap();
    rec.init_record(1).unwrap();

    rec.put_field("ACCL", EpicsValue::Double(2.0)).unwrap();
    assert!((rec.vel.accs - 5.0).abs() < 1e-9, "ACCS={}", rec.vel.accs);
    rec.put_field("ACCS", EpicsValue::Double(4.0)).unwrap();
    assert!((rec.vel.accl - 2.5).abs() < 1e-9, "ACCL={}", rec.vel.accl);
    // Non-positive ACCL floors to 0.1 and re-derives ACCS.
    rec.put_field("ACCL", EpicsValue::Double(-1.0)).unwrap();
    assert!((rec.vel.accl - 0.1).abs() < 1e-9, "ACCL={}", rec.vel.accl);
    assert!((rec.vel.accs - 100.0).abs() < 1e-9, "ACCS={}", rec.vel.accs);
    // Non-positive ACCS is sanitized from ACCL (accs = velo/accl), not to 1.0.
    rec.put_field("ACCS", EpicsValue::Double(0.0)).unwrap();
    assert!((rec.vel.accs - 100.0).abs() < 1e-9, "ACCS={}", rec.vel.accs);
    assert!((rec.vel.accl - 0.1).abs() < 1e-9, "ACCL={}", rec.vel.accl);
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

// --- Resolution triple at init (C check_speed_and_resolution, motorRecord.cc:3904-3927) ---

#[test]
fn test_resolution_triple_default_init() {
    // Nothing configured: C forces MRES = 1.0 (3917-3921) and makes UREV
    // agree, UREV = MRES × SREV = 200 (3922-3927).
    let mut rec = MotorRecord::new();
    rec.init_record(0).unwrap();
    rec.init_record(1).unwrap();

    assert_eq!(rec.conv.srev, 200);
    assert!(
        (rec.conv.mres - 1.0).abs() < 1e-12,
        "MRES={}",
        rec.conv.mres
    );
    assert!(
        (rec.conv.urev - 200.0).abs() < 1e-9,
        "UREV={}",
        rec.conv.urev
    );
}

#[test]
fn test_resolution_init_urev_wins_over_loaded_mres() {
    // C 3911-3916: a nonzero loaded UREV derives MRES — regardless of the
    // order the .db listed the two fields (raw struct writes at load).
    for mres_first in [true, false] {
        let mut rec = MotorRecord::new();
        if mres_first {
            rec.put_field("MRES", EpicsValue::Double(0.1)).unwrap();
            rec.put_field("UREV", EpicsValue::Double(36.0)).unwrap();
        } else {
            rec.put_field("UREV", EpicsValue::Double(36.0)).unwrap();
            rec.put_field("MRES", EpicsValue::Double(0.1)).unwrap();
        }
        rec.init_record(0).unwrap();
        rec.init_record(1).unwrap();

        assert!(
            (rec.conv.mres - 0.18).abs() < 1e-12,
            "mres_first={mres_first} MRES={}",
            rec.conv.mres
        );
        assert!(
            (rec.conv.urev - 36.0).abs() < 1e-12,
            "mres_first={mres_first} UREV={}",
            rec.conv.urev
        );
    }
}

#[test]
fn test_resolution_init_loaded_mres_and_srev_any_order() {
    // No UREV in the .db: the loaded MRES survives at the loaded SREV and
    // UREV is derived (C 3922-3927). Regression: the load-time SREV put
    // used to re-derive MRES from the mid-load UREV, so field(MRES)
    // followed by field(SREV,400) came up with MRES = 0.5×200/400 = 0.25.
    for mres_first in [true, false] {
        let mut rec = MotorRecord::new();
        if mres_first {
            rec.put_field("MRES", EpicsValue::Double(0.5)).unwrap();
            rec.put_field("SREV", EpicsValue::Long(400)).unwrap();
        } else {
            rec.put_field("SREV", EpicsValue::Long(400)).unwrap();
            rec.put_field("MRES", EpicsValue::Double(0.5)).unwrap();
        }
        rec.init_record(0).unwrap();
        rec.init_record(1).unwrap();

        assert_eq!(rec.conv.srev, 400);
        assert!(
            (rec.conv.mres - 0.5).abs() < 1e-12,
            "mres_first={mres_first} MRES={}",
            rec.conv.mres
        );
        assert!(
            (rec.conv.urev - 200.0).abs() < 1e-9,
            "mres_first={mres_first} UREV={}",
            rec.conv.urev
        );
    }
}

#[test]
fn test_resolution_init_nonpositive_srev_clamped() {
    // C 3904-3909: a loaded SREV <= 0 lands raw and init clamps it to 200.
    let mut rec = MotorRecord::new();
    rec.put_field("SREV", EpicsValue::Long(-5)).unwrap();
    rec.put_field("MRES", EpicsValue::Double(0.5)).unwrap();
    rec.init_record(0).unwrap();
    rec.init_record(1).unwrap();

    assert_eq!(rec.conv.srev, 200);
    assert!(
        (rec.conv.mres - 0.5).abs() < 1e-12,
        "MRES={}",
        rec.conv.mres
    );
}

#[test]
fn test_runtime_srev_put_recomputes_mres_only() {
    // C special SREV (2913-2924): make MRES agree and break — UREV, the
    // EGU velocities and the dial limits are all left alone (only the
    // MRES/UREV cases run velcheckB and the limit rescale).
    let mut rec = MotorRecord::new();
    rec.put_field("MRES", EpicsValue::Double(0.5)).unwrap();
    rec.put_field("VELO", EpicsValue::Double(8.0)).unwrap();
    rec.put_field("DHLM", EpicsValue::Double(10.0)).unwrap();
    rec.init_record(0).unwrap();
    rec.init_record(1).unwrap();
    assert!(
        (rec.conv.urev - 100.0).abs() < 1e-9,
        "UREV={}",
        rec.conv.urev
    );
    assert!(
        (rec.limits.rhlm - 20.0).abs() < 1e-9,
        "RHLM={}",
        rec.limits.rhlm
    );

    rec.put_field("SREV", EpicsValue::Long(400)).unwrap();
    assert_eq!(rec.conv.srev, 400);
    assert!(
        (rec.conv.mres - 0.25).abs() < 1e-12,
        "MRES={}",
        rec.conv.mres
    );
    // UREV unchanged (C leaves it; the triple stays consistent).
    assert!(
        (rec.conv.urev - 100.0).abs() < 1e-9,
        "UREV={}",
        rec.conv.urev
    );
    // No velcheckB: VELO/S untouched.
    assert!((rec.vel.velo - 8.0).abs() < 1e-9, "VELO={}", rec.vel.velo);
    assert!((rec.vel.s - 0.08).abs() < 1e-9, "S={}", rec.vel.s);
    // No limit rescale: DHLM stays, raw register stays.
    assert!(
        (rec.limits.dhlm - 10.0).abs() < 1e-9,
        "DHLM={}",
        rec.limits.dhlm
    );
    assert!(
        (rec.limits.rhlm - 20.0).abs() < 1e-9,
        "RHLM={}",
        rec.limits.rhlm
    );
}

#[test]
fn test_runtime_srev_nonpositive_clamped_to_200() {
    // C special SREV (2914-2918): a non-positive runtime put is clamped
    // to 200 (the old handler rejected it and kept the previous SREV).
    let mut rec = MotorRecord::new();
    rec.put_field("MRES", EpicsValue::Double(0.5)).unwrap();
    rec.put_field("SREV", EpicsValue::Long(400)).unwrap();
    rec.init_record(0).unwrap();
    rec.init_record(1).unwrap();
    assert!(
        (rec.conv.urev - 200.0).abs() < 1e-9,
        "UREV={}",
        rec.conv.urev
    );

    rec.put_field("SREV", EpicsValue::Long(-3)).unwrap();
    assert_eq!(rec.conv.srev, 200);
    assert!(
        (rec.conv.mres - 1.0).abs() < 1e-12,
        "MRES={}",
        rec.conv.mres
    );
}

// --- Initial LVIO (C init_record, motorRecord.cc:734-743) ---

#[test]
fn test_initial_lvio_clear_when_limits_disabled() {
    // C 734-737: LVIO is explicitly cleared at init and the violation
    // check is skipped when the dial pair is disabled (DHLM == DLLM ==
    // 0). Regression: the Rust default was lvio: true, so a
    // never-processed record served LVIO = 1.
    let mut rec = MotorRecord::new();
    rec.pos.drbv = 5.0;
    rec.init_record(0).unwrap();
    rec.init_record(1).unwrap();
    assert!(!rec.limits.lvio);
}

#[test]
fn test_initial_lvio_set_when_drbv_outside_limits() {
    // C 738-743: an initial dial readback beyond DHLM + MRES raises
    // LVIO at init, before any processing.
    let mut rec = MotorRecord::new();
    rec.put_field("MRES", EpicsValue::Double(0.5)).unwrap();
    rec.put_field("DHLM", EpicsValue::Double(10.0)).unwrap();
    rec.put_field("DLLM", EpicsValue::Double(-10.0)).unwrap();
    rec.pos.drbv = 10.6;
    rec.init_record(0).unwrap();
    rec.init_record(1).unwrap();
    assert!(rec.limits.lvio, "DRBV=10.6 > DHLM+MRES=10.5 must set LVIO");
}

#[test]
fn test_initial_lvio_respects_mres_slop() {
    // C 738: the comparison allows one MRES of slop — a readback inside
    // DHLM + MRES does not violate.
    let mut rec = MotorRecord::new();
    rec.put_field("MRES", EpicsValue::Double(0.5)).unwrap();
    rec.put_field("DHLM", EpicsValue::Double(10.0)).unwrap();
    rec.put_field("DLLM", EpicsValue::Double(-10.0)).unwrap();
    rec.pos.drbv = 10.4;
    rec.init_record(0).unwrap();
    rec.init_record(1).unwrap();
    assert!(
        !rec.limits.lvio,
        "DRBV=10.4 <= DHLM+MRES=10.5 must not set LVIO"
    );
}

#[test]
fn test_initial_lvio_set_on_inverted_limits() {
    // C 739: DLLM > DHLM raises LVIO at init regardless of the readback.
    let mut rec = MotorRecord::new();
    rec.put_field("MRES", EpicsValue::Double(0.5)).unwrap();
    rec.put_field("DHLM", EpicsValue::Double(-5.0)).unwrap();
    rec.put_field("DLLM", EpicsValue::Double(0.0)).unwrap();
    rec.pos.drbv = -2.0;
    rec.init_record(0).unwrap();
    rec.init_record(1).unwrap();
    assert!(rec.limits.lvio);
}

#[test]
fn test_eres_seeded_from_mres_at_init() {
    // C init_record 692-696: ERES == 0 is seeded from MRES at init —
    // whether the .db never set it or loaded an explicit 0, and
    // regardless of where MRES appeared in the load order. Regression:
    // the load-time ERES put mapped 0 -> the mid-load MRES (default
    // 1.0 when field(ERES) preceded field(MRES)).
    // Unset ERES.
    let mut rec = MotorRecord::new();
    rec.put_field("MRES", EpicsValue::Double(0.25)).unwrap();
    rec.init_record(0).unwrap();
    rec.init_record(1).unwrap();
    assert_eq!(rec.conv.eres, 0.25);

    // Explicit zero ERES loaded before MRES.
    let mut rec = MotorRecord::new();
    rec.put_field("ERES", EpicsValue::Double(0.0)).unwrap();
    rec.put_field("MRES", EpicsValue::Double(0.25)).unwrap();
    rec.init_record(0).unwrap();
    rec.init_record(1).unwrap();
    assert_eq!(rec.conv.eres, 0.25);

    // A configured nonzero ERES survives.
    let mut rec = MotorRecord::new();
    rec.put_field("ERES", EpicsValue::Double(0.5)).unwrap();
    rec.put_field("MRES", EpicsValue::Double(0.25)).unwrap();
    rec.init_record(0).unwrap();
    rec.init_record(1).unwrap();
    assert_eq!(rec.conv.eres, 0.5);
}

#[test]
fn test_runtime_eres_zero_put_maps_to_mres() {
    // C special ERES (2927-2929): a runtime put of 0 maps to MRES.
    let mut rec = MotorRecord::new();
    rec.put_field("MRES", EpicsValue::Double(0.25)).unwrap();
    rec.init_record(0).unwrap();
    rec.init_record(1).unwrap();
    rec.put_field("ERES", EpicsValue::Double(0.002)).unwrap();
    assert_eq!(rec.conv.eres, 0.002);
    rec.put_field("ERES", EpicsValue::Double(0.0)).unwrap();
    assert_eq!(rec.conv.eres, 0.25);
}

#[test]
fn test_resolution_init_zero_mres_forced_to_one() {
    // C 3917-3921: an explicitly loaded MRES of 0 lands raw and init
    // forces 1.0.
    let mut rec = MotorRecord::new();
    rec.put_field("MRES", EpicsValue::Double(0.0)).unwrap();
    rec.init_record(0).unwrap();
    rec.init_record(1).unwrap();

    assert!(
        (rec.conv.mres - 1.0).abs() < 1e-12,
        "MRES={}",
        rec.conv.mres
    );
    assert!(
        (rec.conv.urev - 200.0).abs() < 1e-9,
        "UREV={}",
        rec.conv.urev
    );
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
    // ACCS put does not switch ACCU (R22 / 63bfe5d0); it stays the default Accl.
    assert_eq!(rec.get_field("ACCU"), Some(EpicsValue::Short(0)));
    // ACCU is an independent user/autosave control.
    rec.put_field("ACCU", EpicsValue::Short(1)).unwrap();
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
    // Keep the replayed move single-leg (C case 1 needs BVEL == VELO
    // AND BACC == ACCL) so the replay assertion sees the target itself.
    rec.vel.bacc = rec.vel.accl;
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
fn test_spmg_move_restores_to_pause_on_close_enough() {
    // C maybeRetry close-enough (1096-1101): a motion initiated under
    // SPMG=Move reverts to Pause when the target is reached.
    let mut rec = MotorRecord::new();
    rec.ctrl.spmg = SpmgMode::Move;
    rec.internal.lspg = SpmgMode::Move; // settled, not a fresh edge
    rec.retry.rdbd = 0.1;
    rec.put_field("VAL", EpicsValue::Double(50.0)).unwrap();
    rec.plan_motion(CommandSource::Val);
    assert_eq!(rec.stat.phase, MotionPhase::MainMove);

    rec.stat.msta = MstaFlags::DONE;
    rec.stat.movn = false;
    rec.pos.rbv = 50.0;
    rec.pos.drbv = 50.0;
    rec.check_completion();
    assert_eq!(rec.ctrl.spmg, SpmgMode::Pause, "Move is a one-shot");
    assert_eq!(
        rec.internal.lspg,
        SpmgMode::Pause,
        "restore is not a user transition: the latent-SPMG tracker follows"
    );
}

#[test]
fn test_spmg_move_giveup_miss_keeps_move() {
    // C maybeRetry give-up (1059-1074): too many retries latches MISS
    // and collapses to DONE WITHOUT the Move->Pause restore — C keeps
    // that assignment commented out (1062). Only the close-enough
    // branch restores.
    let mut rec = MotorRecord::new();
    rec.ctrl.spmg = SpmgMode::Move;
    rec.internal.lspg = SpmgMode::Move;
    rec.retry.rtry = 1;
    rec.retry.rdbd = 0.1;
    rec.put_field("VAL", EpicsValue::Double(50.0)).unwrap();
    rec.plan_motion(CommandSource::Val);

    // First completion far from target: arms the retry (rcnt 0 -> 1).
    rec.stat.msta = MstaFlags::DONE;
    rec.stat.movn = false;
    rec.pos.rbv = 45.0;
    rec.pos.drbv = 45.0;
    rec.check_completion();
    assert_eq!(rec.retry.rcnt, 1);

    // Second completion still far: rcnt >= rtry — give up, MISS.
    rec.stat.msta = MstaFlags::DONE;
    rec.stat.movn = false;
    rec.check_completion();
    assert!(rec.retry.miss, "give-up latches MISS");
    assert_eq!(rec.stat.phase, MotionPhase::Idle);
    assert_eq!(
        rec.ctrl.spmg,
        SpmgMode::Move,
        "give-up keeps SPMG=Move (C 1062 restore is commented out)"
    );
}

#[test]
fn test_spmg_move_home_completion_keeps_move() {
    // A home completion goes through postProcess (893-906) and never
    // reaches maybeRetry (process 1427 sees mip == MIP_DONE), so the
    // Move->Pause restore does not apply to it.
    let mut rec = MotorRecord::new();
    rec.ctrl.spmg = SpmgMode::Move;
    rec.internal.lspg = SpmgMode::Move;
    rec.ctrl.homf = true;
    rec.plan_motion(CommandSource::Homf);
    assert_eq!(rec.stat.phase, MotionPhase::Homing);

    rec.stat.msta = MstaFlags::DONE;
    rec.stat.movn = false;
    rec.check_completion();
    assert_eq!(rec.stat.phase, MotionPhase::Idle);
    assert_eq!(
        rec.ctrl.spmg,
        SpmgMode::Move,
        "home completion never reaches the maybeRetry restore"
    );
}

#[test]
fn test_rhls_rlls_raw_limit_readbacks() {
    // C 3727-3734: RHLS/RLLS hold the raw MSTA limit bits unmapped;
    // HLS/LLS derive from them by DIR/MRES polarity. Under inverted
    // polarity the user pair swaps while the raw pair does not.
    let mut rec = MotorRecord::new();
    rec.conv.mres = -1.0; // inverted polarity with DIR=Pos
    let status = asyn_rs::interfaces::motor::MotorStatus {
        high_limit: true,
        low_limit: false,
        done: true,
        ..Default::default()
    };
    rec.process_motor_info(&status);
    assert_eq!(rec.get_field("RHLS"), Some(EpicsValue::Short(1)));
    assert_eq!(rec.get_field("RLLS"), Some(EpicsValue::Short(0)));
    assert_eq!(
        rec.get_field("HLS"),
        Some(EpicsValue::Short(0)),
        "user high maps from raw LOW under inverted polarity"
    );
    assert_eq!(rec.get_field("LLS"), Some(EpicsValue::Short(1)));
}

#[test]
fn test_vers_reports_ported_record_version() {
    // C 196/608: VERS is stamped with the motorRecord VERSION (7.4),
    // SPC_NOMOD.
    let rec = MotorRecord::new();
    assert_eq!(rec.get_field("VERS"), Some(EpicsValue::Float(7.4)));
}

#[test]
fn test_pid_gain_clamp_under_gain_support() {
    // C special pidcof (3003-3014): GAIN_SUPPORT clamps the PID gains
    // to [0,1] at write time.
    let mut rec = MotorRecord::new();
    rec.stat.msta = MstaFlags::GAIN_SUPPORT;
    rec.put_field("PCOF", EpicsValue::Double(1.5)).unwrap();
    rec.put_field("ICOF", EpicsValue::Double(-0.5)).unwrap();
    rec.put_field("DCOF", EpicsValue::Double(0.25)).unwrap();
    assert_eq!(rec.pid.pcof, 1.0);
    assert_eq!(rec.pid.icof, 0.0);
    assert_eq!(rec.pid.dcof, 0.25);
}

#[test]
fn test_pid_gain_raw_store_without_gain_support() {
    // C: the whole pidcof block is inside `if (GAIN_SUPPORT)` — a
    // controller without it stores the raw value, unclamped.
    let mut rec = MotorRecord::new();
    rec.stat.msta = MstaFlags::DONE;
    rec.put_field("PCOF", EpicsValue::Double(1.5)).unwrap();
    assert_eq!(rec.pid.pcof, 1.5);
}

#[test]
fn test_pid_gain_put_forwards_driver_command() {
    // C special pidcof (3003-3026): with GAIN_SUPPORT each coefficient
    // put builds SET_PGAIN/SET_IGAIN/SET_DGAIN with the CLAMPED value
    // and sends it from special(); the pp pass that follows carries it.
    let mut rec = MotorRecord::new();
    rec.stat.msta = MstaFlags::DONE | MstaFlags::GAIN_SUPPORT;
    rec.put_field("PCOF", EpicsValue::Double(1.5)).unwrap();
    rec.put_field("ICOF", EpicsValue::Double(0.3)).unwrap();
    rec.put_field("DCOF", EpicsValue::Double(-0.5)).unwrap();
    let effects = rec.do_process();
    assert_eq!(
        &effects.commands[..3],
        &[
            MotorCommand::SetPidGain {
                kind: PidGainKind::Proportional,
                gain: 1.0,
            },
            MotorCommand::SetPidGain {
                kind: PidGainKind::Integral,
                gain: 0.3,
            },
            MotorCommand::SetPidGain {
                kind: PidGainKind::Derivative,
                gain: 0.0,
            },
        ],
        "put order preserved, values clamped before emission (C 3005-3017)"
    );
}

#[test]
fn test_pid_gain_put_no_forward_without_gain_support() {
    // C: no GAIN_SUPPORT — no build_trans, the put is a raw store.
    let mut rec = MotorRecord::new();
    rec.stat.msta = MstaFlags::DONE;
    rec.put_field("PCOF", EpicsValue::Double(0.5)).unwrap();
    let effects = rec.do_process();
    assert!(
        !effects
            .commands
            .iter()
            .any(|c| matches!(c, MotorCommand::SetPidGain { .. })),
        "no SET_PGAIN without GAIN_SUPPORT"
    );
}

#[test]
fn test_dial_limit_put_forwards_one_sided() {
    // C set_dial_highlimit (4236-4277) / set_dial_lowlimit (4287-4328):
    // a dial-limit put sends exactly ONE limit command. The boundary is
    // dial-frame EGU (like SetPosition), so DHLM maps to SetHighLimit
    // unswapped — C's MRES-sign register swap lives in the raw pair.
    let mut rec = MotorRecord::new();
    rec.conv.mres = 0.01;
    rec.stat.msta = MstaFlags::DONE;
    rec.internal.init_invariants_synced = true;

    rec.put_field("DHLM", EpicsValue::Double(10.0)).unwrap();
    let effects = rec.do_process();
    assert_eq!(
        effects.commands,
        vec![MotorCommand::SetHighLimit { position: 10.0 }]
    );

    rec.put_field("DLLM", EpicsValue::Double(-10.0)).unwrap();
    let effects = rec.do_process();
    assert_eq!(
        effects.commands,
        vec![MotorCommand::SetLowLimit { position: -10.0 }]
    );
}

#[test]
fn test_user_limit_put_folds_dir_into_forwarded_side() {
    // C set_user_highlimit (4076-4154): with DIR=Neg the user HIGH
    // limit lands on the dial LOW side, so the driver forward is the
    // LOW limit (C folds `dir_positive`); DIR=Pos forwards HIGH.
    let mut rec = MotorRecord::new();
    rec.conv.mres = 0.01;
    rec.stat.msta = MstaFlags::DONE;
    rec.internal.init_invariants_synced = true;

    rec.put_field("HLM", EpicsValue::Double(10.0)).unwrap();
    let effects = rec.do_process();
    assert_eq!(
        effects.commands,
        vec![MotorCommand::SetHighLimit { position: 10.0 }]
    );

    rec.conv.dir = MotorDir::Neg;
    rec.put_field("HLM", EpicsValue::Double(10.0)).unwrap();
    let effects = rec.do_process();
    assert_eq!(
        effects.commands,
        vec![MotorCommand::SetLowLimit { position: -10.0 }],
        "user high under DIR=Neg is the dial low limit"
    );
}

#[test]
fn test_limit_put_during_load_does_not_forward() {
    // C: dbLoadRecords applies field() as raw struct writes — special()
    // never runs, so a loaded limit emits no driver command. The queue
    // helper is init-gated like the raw-pair helpers.
    let mut rec = MotorRecord::new();
    rec.conv.mres = 0.01;
    rec.stat.msta = MstaFlags::DONE;
    assert!(!rec.internal.init_invariants_synced);

    rec.put_field("DHLM", EpicsValue::Double(10.0)).unwrap();
    rec.internal.init_invariants_synced = true;
    let effects = rec.do_process();
    assert!(
        !effects.commands.iter().any(|c| matches!(
            c,
            MotorCommand::SetHighLimit { .. } | MotorCommand::SetLowLimit { .. }
        )),
        "load-time limit put must not reach the driver"
    );
}

#[test]
fn test_special_cmds_drain_in_front_of_pass_commands() {
    // C wire order: special() sends fire when the put lands, BEFORE the
    // pp pass enters do_work — a coefficient put followed by a VAL move
    // in the same pass puts SET_PGAIN ahead of the move on the wire.
    let mut rec = MotorRecord::new();
    rec.conv.mres = 0.01;
    rec.limits.dhlm = 100.0;
    rec.limits.dllm = -100.0;
    rec.limits.hlm = 100.0;
    rec.limits.llm = -100.0;
    rec.stat.msta = MstaFlags::DONE | MstaFlags::GAIN_SUPPORT;
    rec.internal.init_invariants_synced = true;

    rec.put_field("PCOF", EpicsValue::Double(0.5)).unwrap();
    rec.put_field("VAL", EpicsValue::Double(10.0)).unwrap();
    let effects = rec.do_process();
    assert_eq!(
        effects.commands[0],
        MotorCommand::SetPidGain {
            kind: PidGainKind::Proportional,
            gain: 0.5,
        }
    );
    assert!(
        effects
            .commands
            .iter()
            .any(|c| matches!(c, MotorCommand::MoveAbsolute { .. })),
        "the pass's own move follows the special() send"
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
            udf: 0,
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
        c.udf = 1;
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

    // C 1427: `mip != MIP_DONE` gates maybeRetry on the post-delay
    // pass. A home completion collapsed MIP via postProcess (894-906)
    // before the delay armed, so its post-delay pass finalizes without
    // the retry evaluation — a latched MISS survives (maybeRetry's
    // close-enough branch, which clears it, is never reached).
    #[test]
    fn home_completion_post_delay_skips_retry_eval() {
        let mut rec = MotorRecord::new();
        rec.conv.mres = 0.1;
        rec.retry.rtry = 3;
        rec.retry.rdbd = 0.5;
        rec.timing.dly = 0.2;
        rec.stat.msta = MstaFlags::DONE;
        rec.put_field("HOMF", EpicsValue::Short(1)).unwrap();
        rec.do_process();
        assert_eq!(rec.stat.phase, MotionPhase::Homing);
        rec.retry.miss = true; // latched by an earlier give-up

        rec.stat.msta = MstaFlags::DONE;
        rec.stat.movn = false;
        let effects = rec.check_completion();
        assert_eq!(rec.stat.phase, MotionPhase::DelayWait);
        assert!(effects.schedule_delay.is_some(), "delay armed");

        rec.set_event(MotorEvent::DelayExpired);
        let _ = rec.do_process();
        let effects = rec.check_completion();
        assert!(
            !effects
                .commands
                .iter()
                .any(|c| matches!(c, MotorCommand::MoveAbsolute { .. })),
            "no retry from a home completion"
        );
        assert!(rec.stat.dmov, "finalized after the settle");
        assert!(rec.stat.mip.is_empty());
        assert!(
            rec.retry.miss,
            "MISS survives: C skips maybeRetry when mip == MIP_DONE (1427)"
        );
        assert_eq!(rec.retry.rcnt, 0);
    }

    // Same key for a commanded stop: C postProcess (826-849) cleared
    // MIP_STOP before the delay, so the post-delay pass never runs
    // maybeRetry — no retry dispatch, MISS untouched.
    #[test]
    fn stop_completion_post_delay_skips_retry_eval() {
        let mut rec = MotorRecord::new();
        rec.conv.mres = 0.1;
        rec.retry.rtry = 3;
        rec.retry.rdbd = 0.5;
        rec.timing.dly = 0.2;
        rec.put_field("VAL", EpicsValue::Double(10.0)).unwrap();
        rec.do_process();
        rec.stat.msta = MstaFlags::MOVING;
        rec.stat.movn = true;
        rec.retry.miss = true; // latched by an earlier give-up

        rec.put_field("STOP", EpicsValue::Short(1)).unwrap();
        rec.do_process();
        assert!(rec.stat.mip.contains(MipFlags::STOP));

        // Axis halts 2.0 short; pp syncs the drive fields, then DLY arms.
        rec.stat.msta = MstaFlags::DONE;
        rec.stat.movn = false;
        rec.pos.drbv = 8.0;
        rec.pos.rbv = 8.0;
        let effects = rec.check_completion();
        assert_eq!(rec.stat.phase, MotionPhase::DelayWait);
        assert!(effects.schedule_delay.is_some(), "delay armed");

        rec.set_event(MotorEvent::DelayExpired);
        let _ = rec.do_process();
        let effects = rec.check_completion();
        assert!(
            !effects
                .commands
                .iter()
                .any(|c| matches!(c, MotorCommand::MoveAbsolute { .. })),
            "no retry from a stop completion"
        );
        assert!(rec.stat.dmov, "finalized after the settle");
        assert!(
            rec.retry.miss,
            "MISS survives: C skips maybeRetry when mip == MIP_DONE (1427)"
        );
        assert_eq!(rec.retry.rcnt, 0);
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

// =============================================================================
// MISS semantics — C maybeRetry is the only writer (1072 set, 1092 clear):
// rtry==0 leaves miss untouched, LS-blocked/close-enough clear it, dispatch
// never touches it
// =============================================================================

mod miss_semantics {
    use super::*;

    fn completed_short(rec: &mut MotorRecord) {
        rec.stat.msta = MstaFlags::DONE;
        rec.stat.phase = MotionPhase::MainMove;
        rec.conv.mres = 0.001;
        rec.retry.rdbd = 0.1;
        rec.pos.dval = 10.0;
        rec.pos.drbv = 9.5; // diff = 0.5 >= rdbd
    }

    // C 1054-1056: rtry == 0 disables retry — mip collapses, miss untouched.
    #[test]
    fn rtry_zero_does_not_latch_miss() {
        let mut rec = MotorRecord::new();
        completed_short(&mut rec);
        rec.retry.rtry = 0;
        rec.check_completion();
        assert_eq!(rec.stat.phase, MotionPhase::Idle);
        assert!(!rec.retry.miss, "rtry==0 must not latch MISS");
    }

    // C 1054-1056: untouched also means a previously latched miss survives.
    #[test]
    fn rtry_zero_preserves_latched_miss() {
        let mut rec = MotorRecord::new();
        completed_short(&mut rec);
        rec.retry.rtry = 0;
        rec.retry.miss = true;
        rec.check_completion();
        assert!(rec.retry.miss, "rtry==0 leaves a latched MISS in place");
    }

    // C 1049 + 1083-1092: a limit switch in the commanded direction routes
    // the evaluation into the close-enough branch, which CLEARS miss.
    #[test]
    fn ls_blocked_clears_miss() {
        let mut rec = MotorRecord::new();
        completed_short(&mut rec);
        rec.retry.rtry = 3;
        rec.retry.miss = true;
        rec.stat.cdir = true; // commanded toward high limit
        rec.limits.hls = true;
        rec.check_completion();
        assert_eq!(rec.stat.phase, MotionPhase::Idle);
        assert!(
            !rec.retry.miss,
            "LS-blocked evaluation clears MISS (C 1092)"
        );
        assert_eq!(rec.retry.rcnt, 0, "no retry dispatched against the limit");
    }

    // C 1083-1092: landing close enough clears a latched miss.
    #[test]
    fn close_enough_clears_miss() {
        let mut rec = MotorRecord::new();
        completed_short(&mut rec);
        rec.pos.drbv = 9.99; // diff = 0.01 < rdbd
        rec.retry.rtry = 3;
        rec.retry.miss = true;
        rec.check_completion();
        assert!(!rec.retry.miss, "close-enough evaluation clears MISS");
    }

    // Dispatch is not a miss writer: a latched miss stays visible through
    // the next move until its own evaluation lands.
    #[test]
    fn dispatch_preserves_latched_miss() {
        let mut rec = MotorRecord::new();
        rec.conv.mres = 0.001;
        rec.retry.miss = true;
        rec.put_field("VAL", EpicsValue::Double(5.0)).unwrap();
        rec.do_process();
        assert_eq!(rec.stat.phase, MotionPhase::MainMove);
        assert!(
            rec.retry.miss,
            "dispatch must not clear MISS (C only writes it in maybeRetry)"
        );
    }

    // Retry branch does not touch miss: a latched miss survives a retry
    // dispatch and clears only when the retry lands close enough.
    #[test]
    fn retry_dispatch_preserves_latched_miss() {
        let mut rec = MotorRecord::new();
        completed_short(&mut rec);
        rec.retry.rtry = 3;
        rec.retry.miss = true;
        rec.check_completion();
        assert_eq!(rec.stat.phase, MotionPhase::MainMove);
        assert!(
            rec.stat.mip.contains(MipFlags::RETRY),
            "retry marked in MIP"
        );
        assert_eq!(rec.retry.rcnt, 1);
        assert!(
            rec.retry.miss,
            "retry branch leaves MISS untouched (C 1077-1082)"
        );
    }
}

// --- C do_work ordering: too_small before LVIO; the toward-valid
// --- exception; RCNT reset ahead of the refusal (2308/2351/2396) ---

#[test]
fn test_sub_step_write_past_limit_completes_without_lvio() {
    // C checks too_small (2308-2348) BEFORE the LVIO evaluation (2396):
    // a request whose raw error is under one step completes quietly even
    // when the target sits past a soft limit.
    let mut rec = MotorRecord::new();
    rec.conv.mres = 0.001;
    rec.put_field("HLM", EpicsValue::Double(10.0)).unwrap();
    rec.put_field("LLM", EpicsValue::Double(-10.0)).unwrap();
    rec.pos.drbv = 10.0;
    rec.pos.rbv = 10.0;
    rec.stat.msta = MstaFlags::DONE;

    // 10.0004 is past DHLM but NINT(0.0004/0.001) rounds to 0 steps.
    rec.put_field("VAL", EpicsValue::Double(10.0004)).unwrap();
    let effects = rec.plan_motion(CommandSource::Val);

    assert!(!rec.limits.lvio, "too_small wins over LVIO (C ordering)");
    assert!(
        !effects
            .commands
            .iter()
            .any(|c| matches!(c, MotorCommand::MoveAbsolute { .. })),
        "no move dispatched"
    );
    assert_eq!(rec.internal.ldvl, 10.0004, "suppressed target adopted");
}

#[test]
fn test_toward_valid_exception_applies_to_non_preferred_move() {
    // C 2403-2405: a target beyond a limit but closer to the valid range
    // than LDVL is allowed — the arm precedes the preferred/non-preferred
    // split, so it is granted on DVAL even when the move would do a
    // backlash leg whose pretarget is also outside.
    let mut rec = MotorRecord::new();
    rec.conv.mres = 0.001;
    rec.vel.velo = 10.0;
    rec.vel.accl = 0.5;
    rec.vel.bvel = 5.0;
    rec.vel.bacc = 0.5;
    rec.put_field("HLM", EpicsValue::Double(10.0)).unwrap();
    rec.put_field("LLM", EpicsValue::Double(-10.0)).unwrap();
    rec.retry.bdst = 1.0; // dval < ldvl with bdst > 0: non-preferred
    rec.pos.drbv = 15.0;
    rec.pos.rbv = 15.0;
    rec.internal.ldvl = 15.0; // stranded above the (lowered) limit
    rec.stat.msta = MstaFlags::DONE;

    rec.put_field("VAL", EpicsValue::Double(12.0)).unwrap();
    let effects = rec.plan_motion(CommandSource::Val);

    assert!(!rec.limits.lvio, "toward-valid move allowed (C 2403-2405)");
    assert!(
        effects
            .commands
            .iter()
            .any(|c| matches!(c, MotorCommand::MoveAbsolute { .. })),
        "move dispatched, got {:?}",
        effects.commands
    );
}

#[test]
fn test_lvio_refused_fresh_move_still_resets_rcnt() {
    // C resets the retry counter (2351-2356) BEFORE the LVIO refusal
    // (2434-2452): a refused fresh move zeroes a latched RCNT.
    let mut rec = MotorRecord::new();
    rec.conv.mres = 0.001;
    rec.put_field("HLM", EpicsValue::Double(10.0)).unwrap();
    rec.put_field("LLM", EpicsValue::Double(-10.0)).unwrap();
    rec.pos.drbv = 0.0;
    rec.stat.msta = MstaFlags::DONE;
    rec.retry.rcnt = 3; // latched from an exhausted retry cycle

    rec.put_field("VAL", EpicsValue::Double(50.0)).unwrap();
    rec.plan_motion(CommandSource::Val);

    assert!(rec.limits.lvio, "target past DHLM refused");
    assert_eq!(rec.retry.rcnt, 0, "RCNT reset precedes the refusal");
}

#[test]
fn test_lvio_refused_write_rolls_back_drive_fields() {
    // C 2434-2452: a refused request restores VAL/DVAL/RVAL from the
    // last dispatched target — the refused value does not stick.
    let mut rec = MotorRecord::new();
    rec.conv.mres = 0.001;
    rec.put_field("HLM", EpicsValue::Double(10.0)).unwrap();
    rec.put_field("LLM", EpicsValue::Double(-10.0)).unwrap();
    rec.stat.msta = MstaFlags::DONE;

    rec.put_field("VAL", EpicsValue::Double(50.0)).unwrap();
    rec.plan_motion(CommandSource::Val);

    assert!(rec.limits.lvio);
    assert_eq!(rec.pos.val, 0.0, "VAL rolled back to LVAL");
    assert_eq!(rec.pos.dval, 0.0, "DVAL rolled back to LDVL");
    assert_eq!(rec.pos.rval, 0, "RVAL rolled back to LRVL");
    assert!(rec.stat.dmov, "no motion started");
}

#[test]
fn test_lvio_refused_retry_collapses_to_done() {
    // C 2441-2451: an armed retry refused by LVIO collapses to MIP_DONE
    // and completes with DMOV = TRUE instead of leaving the record
    // stuck mid-retry. The retry turns non-preferred (dval == ldvl with
    // BDST > 0), so the pretarget check refuses what the original
    // preferred dispatch allowed.
    let mut rec = MotorRecord::new();
    rec.conv.mres = 0.001;
    rec.limits.dhlm = 10.0;
    rec.limits.dllm = -10.0;
    rec.limits.hlm = 10.0;
    rec.limits.llm = -10.0;
    rec.vel.velo = 10.0;
    rec.vel.accl = 0.5;
    rec.vel.bvel = 5.0;
    rec.vel.bacc = 0.5;
    rec.retry.bdst = 1.0;
    rec.retry.rdbd = 0.1;
    rec.retry.rtry = 5;
    rec.stat.msta = MstaFlags::DONE;
    // Stranded below the valid range: the move up to -9.5 is preferred,
    // so its LVIO pass checks DVAL only (C 2410) and dispatches the
    // two-leg correction through the out-of-range pretarget -10.5.
    rec.pos.drbv = -12.0;
    rec.pos.rbv = -12.0;
    rec.internal.ldvl = -12.0;
    rec.internal.lval = -12.0;

    rec.put_field("VAL", EpicsValue::Double(-9.5)).unwrap();
    rec.plan_motion(CommandSource::Val);
    assert!(!rec.limits.lvio, "preferred dispatch allowed");
    assert!(rec.internal.backlash_pending);

    // Both legs run; the final approach stops short by 0.2 (> RDBD).
    for stop_at in [-10.5, -9.7] {
        let status = asyn_rs::interfaces::motor::MotorStatus {
            position: stop_at,
            encoder_position: stop_at,
            done: true,
            moving: false,
            ..Default::default()
        };
        rec.process_motor_info(&status);
        rec.check_completion();
    }

    assert!(rec.limits.lvio, "retry pretarget past DLLM refused");
    assert!(
        !rec.stat.mip.contains(MipFlags::RETRY),
        "RETRY collapsed to DONE (C 2441-2445)"
    );
    assert!(rec.stat.mip.is_empty());
    assert!(rec.stat.dmov, "refused retry completes (C 2446-2451)");
    assert_eq!(rec.stat.phase, MotionPhase::Idle);
    assert_eq!(rec.retry.rcnt, 1, "the armed retry kept its count");
}

#[test]
fn test_jog_with_inverted_limits_is_rejected() {
    // C 2089: DLLM > DHLM refuses the jog in either direction.
    let mut rec = MotorRecord::new();
    rec.limits.dhlm = -5.0;
    rec.limits.dllm = 5.0;
    rec.limits.hlm = -5.0;
    rec.limits.llm = 5.0;
    rec.vel.jvel = 1.0;
    rec.pos.val = 0.0;
    rec.ctrl.jogf = true;
    let effects = rec.plan_motion(CommandSource::Jogf);
    assert!(
        !effects
            .commands
            .iter()
            .any(|c| matches!(c, MotorCommand::MoveVelocity { .. })),
        "inverted limits refuse the jog"
    );
    assert!(rec.limits.lvio);
    assert!(!rec.ctrl.jogf);
}

#[test]
fn test_jog_limit_disable_gate_is_dial_frame() {
    // C 2085-2086: the disabled-limits gate tests DHLM == DLLM == 0.0 —
    // the DIAL pair. A nonzero OFF leaves the user pair offset from
    // zero, but the limits are still disabled.
    let mut rec = MotorRecord::new();
    rec.limits.dhlm = 0.0;
    rec.limits.dllm = 0.0;
    rec.pos.off = 3.0;
    rec.limits.hlm = 3.0;
    rec.limits.llm = 3.0;
    rec.vel.jvel = 1.0;
    rec.pos.val = 50.0;
    rec.ctrl.jogf = true;
    let effects = rec.plan_motion(CommandSource::Jogf);
    assert!(
        effects
            .commands
            .iter()
            .any(|c| matches!(c, MotorCommand::MoveVelocity { .. })),
        "disabled dial limits allow the jog, got {:?}",
        effects.commands
    );
    assert!(!rec.limits.lvio);
}

#[test]
fn test_jog_refusal_clears_both_buttons() {
    // C 2095-2104: the refusal releases BOTH buttons, not just the
    // pressed one.
    let mut rec = MotorRecord::new();
    rec.put_field("HLM", EpicsValue::Double(10.0)).unwrap();
    rec.put_field("LLM", EpicsValue::Double(-10.0)).unwrap();
    rec.vel.jvel = 1.0;
    rec.pos.val = 9.5;
    rec.ctrl.jogf = true;
    rec.ctrl.jogr = true; // both held (e.g. racing writes)
    rec.plan_motion(CommandSource::Jogf);
    assert!(rec.limits.lvio);
    assert!(!rec.ctrl.jogf, "JOGF released");
    assert!(!rec.ctrl.jogr, "JOGR released too (C 2100-2104)");
}

#[test]
fn test_alarm_cycle_fans_out_alarm_mask_to_monitored_fields() {
    // C `monitor()` (motorRecord.cc:3513-3645): on a cycle whose alarm
    // transition fired, every field in the posting list goes out even
    // when unmarked — `local_mask = monitor_mask | (MARKED(x) ?
    // DBE_VAL_LOG : 0)` is non-zero for unmarked fields once
    // `monitor_mask != 0`. A subscriber on DMOV must observe the alarm
    // moment with a DBE_ALARM-only event even though DMOV itself did
    // not change.
    use epics_base_rs::server::recgbl::EventMask;
    use epics_base_rs::server::record::RecordInstance;
    use epics_base_rs::types::DbFieldType;

    let mut rec = MotorRecord::new();
    rec.stat.msta = MstaFlags::DONE;
    let mut instance = RecordInstance::new("M1".into(), rec);
    // A record is born UDF/`STAT=UDF` (dbCommon.dbd `initial("UDF")`), and
    // clearing that on the first process IS an alarm transition. This test is
    // about a QUIESCENT cycle, so start from the already-processed state:
    // defined value, alarm committed at NO_ALARM.
    instance.common.udf = 0;
    instance.common.stat = epics_base_rs::server::recgbl::alarm_status::NO_ALARM;

    let _dmov_rx = instance
        .add_subscriber(
            "DMOV",
            1,
            DbFieldType::Char,
            (EventMask::VALUE | EventMask::LOG | EventMask::ALARM).bits(),
        )
        .expect("DMOV subscriber");

    // Quiescent pass: no alarm transition, DMOV unchanged — no DMOV post.
    let (snap, _) = instance.process_local().unwrap();
    assert!(
        !snap.changed_fields.iter().any(|(k, _, _)| k == "DMOV"),
        "no alarm and no change: DMOV must not post"
    );

    // Raise a motor alarm without touching DMOV: MSTA RA_PROBLEM drives
    // STATE_ALARM/MAJOR in the alarm hook (C `alarm_sub`,
    // motorRecord.cc:3400-3403).
    instance
        .record
        .as_any_mut()
        .and_then(|a| a.downcast_mut::<MotorRecord>())
        .expect("motor record downcast")
        .stat
        .msta
        .insert(MstaFlags::PROBLEM);
    let (snap, _) = instance.process_local().unwrap();
    let dmov_mask = snap
        .changed_fields
        .iter()
        .find(|(k, _, _)| k == "DMOV")
        .map(|(_, _, m)| *m);
    assert_eq!(
        dmov_mask,
        Some(EventMask::ALARM),
        "alarm cycle posts unchanged listed fields with DBE_ALARM alone"
    );
}

#[test]
fn test_same_value_unblinked_pass_after_miss_giveup_refuses_with_get_info() {
    // C do_work entry gates (motorRecord.cc:2204, 2240) refuse a pass
    // that arrives WITHOUT the special() pass-0 blink — the closed-loop
    // DOL collection (a bare dbGetLink into VAL, 1994, no special) and
    // housekeeping passes. After a retry give-up (maybeRetry 1060-1075
    // adopts the unreached target into lval/ldvl/lrvl with MISS
    // latched), a same-value unblinked delivery fails `val != lval` and
    // `dval != ldvl || !dmov` — no new move dispatches; the pass falls
    // to the chain end and fires the implicit GET_INFO (2546-2557).
    let mut rec = MotorRecord::new();
    rec.set_device_state(motor_rs::device_state::new_shared_state());
    rec.stat.msta = MstaFlags::DONE;
    rec.stat.phase = MotionPhase::MainMove;
    rec.stat.dmov = false;
    rec.conv.mres = 0.001;
    rec.retry.rdbd = 0.1;
    rec.retry.rtry = 3;
    rec.retry.rcnt = 3; // exhausted
    rec.pos.val = 10.0;
    rec.pos.dval = 10.0;
    rec.pos.drbv = 9.5; // error 0.5 > rdbd -> give-up

    let _ = rec.check_completion();
    assert!(rec.retry.miss, "give-up latches MISS");
    assert!(rec.stat.dmov);
    assert_eq!(rec.internal.ldvl, 10.0, "lasts adopt the unreached target");

    // Same-value delivery with no blink (no dbPut ran).
    let effects = rec.plan_motion(CommandSource::Val);
    assert!(
        effects.commands.is_empty(),
        "C 2240: same-value unblinked pass does not re-dispatch the move"
    );
    assert!(
        rec.retry.miss,
        "MISS stays latched (only maybeRetry clears it)"
    );
    assert!(rec.stat.dmov, "no DMOV pulse on the refused pass");
    assert!(
        effects.status_refresh,
        "chain end fires the implicit GET_INFO"
    );
    assert_eq!(rec.stat.stup, 2, "STUP goes BUSY for the refresh");
}

#[test]
fn test_same_value_dbput_after_miss_giveup_redispatches() {
    // A database put is preceded by the special() pass-0 blink
    // (motorRecord.cc:2582-2608): DMOV drops before the record
    // processes, the entry gate (2240) passes via `!dmov`, the target
    // sits 500 steps out (not too_small), and the dispatch gate (2455,
    // `mip == MIP_DONE`) sends the move again — C retries a missed
    // target on an operator re-put of the same value. Anchored first:
    // in C a dbPut pass can only exist after init_record, and an
    // unanchored mailbox-wired record parks every pass
    // (do_process_inner's anchor-first gate), which would swallow the
    // re-dispatch this test asserts on.
    let mut rec = MotorRecord::new();
    let state = motor_rs::device_state::new_shared_state();
    rec.set_device_state(state.clone());
    anchor(&mut rec, &state);
    rec.stat.msta = MstaFlags::DONE;
    rec.stat.phase = MotionPhase::MainMove;
    rec.stat.dmov = false;
    rec.conv.mres = 0.001;
    rec.retry.rdbd = 0.1;
    rec.retry.rtry = 3;
    rec.retry.rcnt = 3; // exhausted
    rec.pos.val = 10.0;
    rec.pos.dval = 10.0;
    rec.pos.drbv = 9.5; // error 0.5 > rdbd -> give-up

    let _ = rec.check_completion();
    assert!(rec.retry.miss, "give-up latches MISS");
    assert!(rec.stat.dmov);

    // dbPut of the same VAL: framework order is special(pass 0), then
    // put_field, then the pp process pass.
    rec.special("VAL", false).unwrap();
    assert!(!rec.stat.dmov, "pass-0 blink drops DMOV");
    rec.put_field("VAL", EpicsValue::Double(10.0)).unwrap();
    let effects = rec.do_process();
    assert!(
        effects
            .commands
            .iter()
            .any(|c| matches!(c, MotorCommand::MoveAbsolute { .. })),
        "C 2455: blinked same-value re-put re-dispatches the missed move"
    );
    assert!(!rec.stat.dmov, "move in flight");
    assert_eq!(rec.stat.phase, MotionPhase::MainMove);
    assert!(
        rec.retry.miss,
        "MISS stays latched until maybeRetry lands close enough"
    );
}

#[test]
fn test_new_target_after_miss_giveup_dispatches() {
    // The give-up only adopts the OLD target into the lasts — a fresh
    // target re-enters the move block via `dval != ldvl` (C 2240).
    let mut rec = MotorRecord::new();
    rec.stat.msta = MstaFlags::DONE;
    rec.stat.phase = MotionPhase::MainMove;
    rec.stat.dmov = false;
    rec.conv.mres = 0.001;
    rec.retry.rdbd = 0.1;
    rec.retry.rtry = 3;
    rec.retry.rcnt = 3; // exhausted
    rec.pos.val = 10.0;
    rec.pos.dval = 10.0;
    rec.pos.drbv = 9.5;

    let _ = rec.check_completion();
    assert!(rec.retry.miss);

    rec.pos.val = 11.0;
    rec.pos.dval = 11.0;
    let effects = rec.plan_motion(CommandSource::Val);
    assert!(
        effects
            .commands
            .iter()
            .any(|c| matches!(c, MotorCommand::MoveAbsolute { .. })),
        "a new target dispatches normally after the give-up"
    );
    assert!(!rec.stat.dmov);
    assert_eq!(rec.stat.phase, MotionPhase::MainMove);
}

#[test]
fn test_same_value_set_redefinition_resends_load_pos_when_blinked() {
    // C: a dbPut to DVAL is preceded by the special() pass-0 blink
    // (2582-2608) even in SET mode, so a same-value redefinition re-put
    // re-enters the move block via `!dmov` (2240) and the set test
    // (2257-2263) routes it to load_pos again — the redefinition is a
    // command, not a value change, and C re-sends LOAD_POS for it.
    // Without the blink (no fresh drive write) the entry gate refuses
    // and the pass falls to the chain end instead.
    let mut rec = MotorRecord::new();
    rec.set_device_state(motor_rs::device_state::new_shared_state());
    rec.conv.mres = 1.0;
    rec.stat.msta = MstaFlags::DONE;
    rec.pos.val = 5.0;
    rec.pos.dval = 5.0;
    rec.pos.drbv = 5.0;
    rec.internal.lval = 5.0;
    rec.internal.ldvl = 5.0;
    rec.put_field("SET", EpicsValue::Short(1)).unwrap();

    rec.special("DVAL", false).unwrap();
    rec.put_field("DVAL", EpicsValue::Double(0.0)).unwrap();
    let effects = rec.plan_motion(CommandSource::Set);
    assert!(
        effects
            .commands
            .iter()
            .any(|c| matches!(c, MotorCommand::SetPosition { .. })),
        "first redefinition dispatches LOAD_POS"
    );
    let status = asyn_rs::interfaces::motor::MotorStatus {
        position: 0.0,
        done: true,
        ..Default::default()
    };
    rec.process_motor_info(&status);
    let _ = rec.check_completion();
    assert!(rec.stat.dmov, "LOAD_P cycle completed");

    // Same-value re-put, blinked like every dbPut.
    rec.special("DVAL", false).unwrap();
    rec.put_field("DVAL", EpicsValue::Double(0.0)).unwrap();
    let effects = rec.plan_motion(CommandSource::Set);
    assert!(
        effects
            .commands
            .iter()
            .any(|c| matches!(c, MotorCommand::SetPosition { .. })),
        "C re-sends LOAD_POS on a blinked same-value redefinition re-put"
    );

    // Unblinked same-value pass (no fresh drive write): entry gate
    // (2240) refuses — chain end, implicit GET_INFO.
    let status = asyn_rs::interfaces::motor::MotorStatus {
        position: 0.0,
        done: true,
        ..Default::default()
    };
    rec.process_motor_info(&status);
    let _ = rec.check_completion();
    assert!(rec.stat.dmov, "second LOAD_P cycle completed");
    let effects = rec.plan_motion(CommandSource::Set);
    assert!(
        effects.commands.is_empty(),
        "no LOAD_POS on an unblinked same-value pass"
    );
    assert!(
        effects.status_refresh,
        "chain end fires the implicit GET_INFO"
    );
}

#[test]
fn test_unblinked_zero_tweak_refuses_and_falls_to_chain_end() {
    // An UNBLINKED zero fold (TWV = 0, no dbPut ran): the fold leaves
    // VAL unchanged, the collection gate (2204) and the move-block
    // entry (2240) both refuse, and the pass ends at the chain end
    // (implicit GET_INFO, 2546-2557) — no move, no DMOV pulse.
    let mut rec = MotorRecord::new();
    rec.set_device_state(motor_rs::device_state::new_shared_state());
    rec.stat.msta = MstaFlags::DONE;
    rec.conv.mres = 0.001;
    rec.pos.val = 10.0;
    rec.pos.dval = 10.0;
    rec.pos.drbv = 10.0;
    rec.internal.lval = 10.0;
    rec.internal.ldvl = 10.0;
    rec.ctrl.twv = 0.0;
    rec.ctrl.twf = true;

    let effects = rec.plan_motion(CommandSource::Twf);
    assert!(effects.commands.is_empty(), "zero fold dispatches nothing");
    assert!(rec.stat.dmov, "no DMOV pulse on the refused pass");
    assert!(
        effects.status_refresh,
        "chain end fires the implicit GET_INFO"
    );
}

#[test]
fn test_blinked_zero_tweak_pulses_dmov() {
    // A dbPut to TWF is preceded by the special() pass-0 blink
    // (2582-2608, TWF is in the blink list), so even a zero fold
    // (TWV = 0) enters the move block via `!dmov` (2240); at position
    // the request is too_small and the sub-step pulse runs — DMOV
    // 1→0→1, no controller command (C 2333-2342 restores DMOV=TRUE on
    // the same pass; the Rust pulse posts the low half and the
    // recovery pass restores it). Anchored first: in C a TWF put can
    // only exist after init_record, and an unanchored mailbox-wired
    // record parks every pass (do_process_inner's anchor-first gate),
    // which would swallow the recovery pass this test asserts on.
    let mut rec = MotorRecord::new();
    let state = motor_rs::device_state::new_shared_state();
    rec.set_device_state(state.clone());
    anchor(&mut rec, &state);
    rec.stat.msta = MstaFlags::DONE;
    rec.conv.mres = 0.001;
    rec.pos.val = 10.0;
    rec.pos.dval = 10.0;
    rec.pos.drbv = 10.0;
    rec.internal.lval = 10.0;
    rec.internal.ldvl = 10.0;
    rec.ctrl.twv = 0.0;

    rec.special("TWF", false).unwrap();
    assert!(!rec.stat.dmov, "pass-0 blink drops DMOV");
    rec.ctrl.twf = true;
    let effects = rec.plan_motion(CommandSource::Twf);
    assert!(effects.commands.is_empty(), "zero fold sends no command");
    assert!(!rec.stat.dmov, "pulse low half");
    assert!(rec.stat.movn, "pulse presents as a (zero-length) move");
    assert!(effects.request_poll, "pulse schedules the recovery pass");

    // The recovery pass (no pending put) restores DMOV=1.
    let _ = rec.do_process();
    assert!(rec.stat.dmov, "pulse high half restores DMOV");
    assert!(!rec.stat.movn);
}

#[test]
fn test_special_pass0_blinks_dmov_for_drive_fields_only() {
    // C special() pass-0 (motorRecord.cc:2582-2608): exactly the six
    // drive fields blink DMOV; nothing else does, and the after-put
    // pass (pass 1) never blinks.
    for f in ["VAL", "DVAL", "RVAL", "RLV", "TWF", "TWR"] {
        let mut rec = MotorRecord::new();
        assert!(rec.stat.dmov);
        rec.special(f, false).unwrap();
        assert!(!rec.stat.dmov, "pass-0 blink drops DMOV for {f}");
    }
    let mut rec = MotorRecord::new();
    rec.special("VAL", true).unwrap();
    assert!(rec.stat.dmov, "after-put special does not blink");
    for f in ["TWV", "SET", "SPMG", "STOP", "HOMF", "JOGF", "VELO"] {
        rec.special(f, false).unwrap();
        assert!(rec.stat.dmov, "{f} is not a drive field — no blink");
    }
}

#[test]
fn test_init_record_pass1_restores_dmov_after_load_time_blink() {
    // C init_record (721-723): dmov = TRUE, movn = FALSE no matter
    // what the load wrote — a load-time write to a drive field may
    // have run the pass-0 blink, and DMOV must still start done.
    let mut rec = MotorRecord::new();
    rec.special("VAL", false).unwrap();
    rec.put_field("VAL", EpicsValue::Double(1.0)).unwrap();
    assert!(!rec.stat.dmov, "load-time blink left DMOV low");
    rec.init_record(1).unwrap();
    assert!(rec.stat.dmov, "init pass 1 restores DMOV=1 (C 721)");
    assert!(!rec.stat.movn, "init pass 1 clears MOVN (C 723)");
}

#[test]
fn test_blinked_put_blocked_by_hw_limit_completes_with_lasts_adopted() {
    // A blinked put whose first leg is blocked by an active hardware
    // limit switch: C dispatches it (no hw-limit gate in the move
    // block), the driver stops at the switch, and maybeRetry's
    // limit-switch escape (1049) completes it with the lasts already
    // adopted at dispatch (2469-2471). The Rust refusal skips the
    // controller command but lands the same end state — the written
    // target stays in VAL/DVAL, the lasts adopt it (no dangling
    // re-dispatch on a later SPMG pass), and the pending pulse
    // completes with DMOV=1 (the blink must not stay latched low).
    let mut rec = MotorRecord::new();
    rec.stat.msta = MstaFlags::DONE;
    rec.conv.mres = 0.001;
    rec.limits.hls = true; // parked on the high limit switch

    rec.special("VAL", false).unwrap();
    rec.put_field("VAL", EpicsValue::Double(10.0)).unwrap();
    let effects = rec.do_process();
    assert!(effects.commands.is_empty(), "blocked move sends nothing");
    assert!(rec.stat.dmov, "refusal completes the pending pulse");
    assert_eq!(rec.stat.phase, MotionPhase::Idle);
    assert_eq!(rec.pos.val, 10.0, "the written target stays (C parity)");
    assert_eq!(rec.internal.ldvl, rec.pos.dval, "lasts adopt the target");

    // No dangling re-dispatch: an SPMG=Go pass finds nothing pending.
    rec.ctrl.spmg = SpmgMode::Go;
    let effects = rec.plan_motion(CommandSource::Spmg);
    assert!(
        effects.commands.is_empty(),
        "no stale target dispatches after the refusal"
    );
}
