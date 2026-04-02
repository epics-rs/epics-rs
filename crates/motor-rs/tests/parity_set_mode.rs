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
fn set_mode_val_produces_set_position() {
    let mut rec = make_record();
    rec.pos.dval = 10.0;
    rec.pos.val = 10.0;
    rec.conv.set = true;

    rec.put_field("VAL", EpicsValue::Double(100.0)).unwrap();
    let effects = rec.plan_motion(CommandSource::Set);

    // Should produce SetPosition command
    assert_eq!(effects.commands.len(), 1);
    assert!(matches!(
        effects.commands[0],
        MotorCommand::SetPosition { .. }
    ));

    // No DMOV change, no phase change
    assert!(rec.stat.dmov);
    assert_eq!(rec.stat.phase, MotionPhase::Idle);
    assert!(!rec.stat.movn);

    // DVAL unchanged, OFF updated
    assert_eq!(rec.pos.dval, 10.0);
    assert_eq!(rec.pos.off, 90.0); // 100 - 1*10
}

#[test]
fn set_mode_dval_produces_set_position() {
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
    assert!(rec.stat.dmov);
    assert_eq!(rec.stat.phase, MotionPhase::Idle);
}

#[test]
fn dir_change_recalculates_user_coords() {
    let mut rec = make_record();
    rec.pos.dval = 10.0;
    rec.pos.drbv = 10.0;
    rec.pos.val = 10.0;
    rec.pos.rbv = 10.0;
    rec.pos.off = 0.0;

    // Change DIR to Neg
    rec.put_field("DIR", EpicsValue::Short(1)).unwrap();

    // VAL = dir.sign() * DVAL + OFF = -1*10 + 0 = -10
    assert_eq!(rec.pos.val, -10.0);
    assert_eq!(rec.pos.rbv, -10.0);
    // DVAL, DRBV unchanged
    assert_eq!(rec.pos.dval, 10.0);
    assert_eq!(rec.pos.drbv, 10.0);
    // Limits recalculated
    let (hlm, llm) = (rec.limits.hlm, rec.limits.llm);
    assert!(hlm >= llm);
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
fn foff_frozen_val_write_preserves_dval() {
    let mut rec = make_record();
    rec.conv.foff = FreezeOffset::Frozen;
    rec.pos.val = 10.0;
    rec.pos.dval = 10.0;
    rec.pos.off = 0.0;

    // Write VAL=20: FOFF=Frozen → DVAL stays 10, OFF adjusts
    rec.put_field("VAL", EpicsValue::Double(20.0)).unwrap();

    assert_eq!(rec.pos.dval, 10.0); // preserved
    assert_eq!(rec.pos.val, 20.0);
    assert_eq!(rec.pos.off, 10.0); // 20 - 1*10
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
fn foff_frozen_dval_write_preserves_val() {
    let mut rec = make_record();
    rec.conv.foff = FreezeOffset::Frozen;
    rec.pos.val = 10.0;
    rec.pos.dval = 5.0;
    rec.pos.off = 5.0;

    // Write DVAL=20: FOFF=Frozen → VAL stays 10, OFF adjusts
    rec.put_field("DVAL", EpicsValue::Double(20.0)).unwrap();

    assert_eq!(rec.pos.val, 10.0); // preserved
    assert_eq!(rec.pos.dval, 20.0);
    assert_eq!(rec.pos.off, -10.0); // 10 - 1*20
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
