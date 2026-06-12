//! C parity tests for SET/OFF/DIR/FOFF semantics.

use motor_rs::flags::*;
use motor_rs::record::MotorRecord;

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
    rec.stat.msta = MstaFlags::DONE;
    rec
}

#[test]
fn set_mode_val_foff_variable_redefines_without_command() {
    let mut rec = make_record();
    rec.pos.dval = 10.0;
    rec.pos.val = 10.0;
    rec.conv.set = true;

    rec.put_field("VAL", EpicsValue::Double(100.0)).unwrap();

    // C 2206-2227: SET + FOFF=Variable VAL acts directly in the
    // collection block — offset adjusted, nothing dispatched (no
    // LOAD_POS), mip = MIP_DONE, dmov = TRUE.
    assert!(rec.stat.dmov);
    assert_eq!(rec.stat.phase, MotionPhase::Idle);
    assert!(!rec.stat.movn);
    assert!(rec.stat.mip.is_empty());

    // DVAL unchanged, OFF updated
    assert_eq!(rec.pos.dval, 10.0);
    assert_eq!(rec.pos.off, 90.0); // 100 - 1*10
}

#[test]
fn set_mode_dval_load_pos_produces_set_position() {
    let mut rec = make_record();
    rec.pos.dval = 10.0;
    rec.pos.val = 10.0;
    rec.conv.set = true;

    rec.put_field("DVAL", EpicsValue::Double(50.0)).unwrap();
    let effects = rec.plan_motion(CommandSource::Set);

    assert_eq!(effects.commands.len(), 1);
    assert!(matches!(
        effects.commands[0],
        MotorCommand::SetPosition { .. }
    ));
    // C load_pos (3802-3808): MIP_LOAD_P, DMOV pulses low, GET_INFO
    // follows the LOAD_POS.
    assert!(!rec.stat.dmov);
    assert_eq!(rec.stat.mip, MipFlags::LOAD_P);
    assert!(effects.request_poll);
    assert_eq!(rec.stat.phase, MotionPhase::Idle);
}

#[test]
fn dir_change_foff_variable_preserves_val() {
    let mut rec = make_record();
    rec.pos.dval = 10.0;
    rec.pos.drbv = 10.0;
    rec.pos.val = 10.0;
    rec.pos.rbv = 10.0;
    rec.pos.off = 0.0;
    // FOFF defaults to Variable

    // Change DIR to Neg
    rec.put_field("DIR", EpicsValue::Short(1)).unwrap();

    // C: FOFF=Variable → OFF is recalculated to preserve VAL
    // OFF = VAL - dir.sign() * DVAL = 10 - (-1)*10 = 20
    assert_eq!(rec.pos.val, 10.0); // preserved
    assert_eq!(rec.pos.off, 20.0);
    // RBV = dir.sign() * DRBV + OFF = -1*10 + 20 = 10
    assert_eq!(rec.pos.rbv, 10.0);
    // DVAL, DRBV unchanged
    assert_eq!(rec.pos.dval, 10.0);
    assert_eq!(rec.pos.drbv, 10.0);
    // Limits recalculated
    let (hlm, llm) = (rec.limits.hlm, rec.limits.llm);
    assert!(hlm >= llm);
}

#[test]
fn dir_change_foff_frozen_recalculates_val() {
    let mut rec = make_record();
    rec.pos.dval = 10.0;
    rec.pos.drbv = 10.0;
    rec.pos.val = 10.0;
    rec.pos.rbv = 10.0;
    rec.pos.off = 0.0;
    rec.conv.foff = FreezeOffset::Frozen;

    // Change DIR to Neg
    rec.put_field("DIR", EpicsValue::Short(1)).unwrap();

    // C: FOFF=Frozen → VAL is recalculated from DVAL
    // VAL = dir.sign() * DVAL + OFF = -1*10 + 0 = -10
    assert_eq!(rec.pos.val, -10.0);
    assert_eq!(rec.pos.rbv, -10.0);
    assert_eq!(rec.pos.off, 0.0); // unchanged
}

#[test]
fn off_change_recalculates_user_coords() {
    let mut rec = make_record();
    rec.pos.dval = 10.0;
    rec.pos.drbv = 10.0;
    rec.pos.val = 10.0;
    rec.pos.rbv = 10.0;
    rec.pos.off = 0.0;

    rec.put_field("OFF", EpicsValue::Double(5.0)).unwrap();

    // VAL = dir.sign() * DVAL + OFF = 1*10 + 5 = 15
    assert_eq!(rec.pos.val, 15.0);
    assert_eq!(rec.pos.rbv, 15.0);
    // DVAL, DRBV unchanged
    assert_eq!(rec.pos.dval, 10.0);
    assert_eq!(rec.pos.drbv, 10.0);
}

#[test]
fn foff_frozen_val_write_cascades_normally() {
    let mut rec = make_record();
    rec.conv.foff = FreezeOffset::Frozen;
    rec.pos.val = 10.0;
    rec.pos.dval = 10.0;
    rec.pos.off = 0.0;

    // C: FOFF has no effect in non-SET mode -- VAL cascades to DVAL normally
    rec.put_field("VAL", EpicsValue::Double(20.0)).unwrap();

    assert_eq!(rec.pos.dval, 20.0); // cascaded normally
    assert_eq!(rec.pos.val, 20.0);
    assert_eq!(rec.pos.off, 0.0); // unchanged
}

#[test]
fn foff_variable_val_write_changes_dval() {
    let mut rec = make_record();
    rec.conv.foff = FreezeOffset::Variable;
    rec.pos.val = 10.0;
    rec.pos.dval = 10.0;
    rec.pos.off = 0.0;

    rec.put_field("VAL", EpicsValue::Double(20.0)).unwrap();

    assert_eq!(rec.pos.dval, 20.0); // changed
    assert_eq!(rec.pos.off, 0.0); // unchanged
}

#[test]
fn foff_frozen_dval_write_cascades_normally() {
    let mut rec = make_record();
    rec.conv.foff = FreezeOffset::Frozen;
    rec.pos.val = 10.0;
    rec.pos.dval = 5.0;
    rec.pos.off = 5.0;

    // C: FOFF has no effect in non-SET mode -- VAL recalculated normally
    rec.put_field("DVAL", EpicsValue::Double(20.0)).unwrap();

    assert_eq!(rec.pos.val, 25.0); // dial_to_user(20.0, Pos, 5.0)
    assert_eq!(rec.pos.dval, 20.0);
    assert_eq!(rec.pos.off, 5.0); // unchanged
}

#[test]
fn off_change_recalculates_limits() {
    let mut rec = make_record();
    rec.limits.dhlm = 50.0;
    rec.limits.dllm = -50.0;
    rec.pos.off = 0.0;

    rec.put_field("OFF", EpicsValue::Double(10.0)).unwrap();

    // HLM = dir.sign() * DHLM + OFF = 1*50 + 10 = 60
    // LLM = dir.sign() * DLLM + OFF = 1*(-50) + 10 = -40
    assert_eq!(rec.limits.hlm, 60.0);
    assert_eq!(rec.limits.llm, -40.0);
}

#[test]
fn dir_neg_off_change() {
    let mut rec = make_record();
    rec.conv.dir = MotorDir::Neg;
    rec.pos.dval = 10.0;
    rec.pos.drbv = 10.0;
    rec.pos.val = -10.0; // DIR=Neg: val = -1*10 + 0
    rec.pos.rbv = -10.0;
    rec.pos.off = 0.0;

    rec.put_field("OFF", EpicsValue::Double(5.0)).unwrap();

    // VAL = -1*10 + 5 = -5
    assert_eq!(rec.pos.val, -5.0);
    assert_eq!(rec.pos.rbv, -5.0);
}
