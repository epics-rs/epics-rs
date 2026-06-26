//! C-parity integration tests for the analog / calc record family.
//!
//! Covers parity-review 06 findings: H1 (longout/int64out drive-limit
//! clamp), M1 (calcout ODLY/DLYA), M3 (ao IVOA=2 re-convert), M5
//! (calc/calcout LA..LU change-gated update), L1 (calc AFVL clearing),
//! L3 (ai/ao breakpoint-table BPT alarm), and the scalcout/transform
//! internal-correctness items S1-S6.

#![allow(clippy::all)]

use epics_base_rs::server::record::*;
use epics_base_rs::server::records::ai::AiRecord;
use epics_base_rs::server::records::ao::AoRecord;
use epics_base_rs::server::records::calc::CalcRecord;
use epics_base_rs::server::records::calcout::CalcoutRecord;
use epics_base_rs::server::records::int64out::Int64outRecord;
use epics_base_rs::server::records::longout::LongoutRecord;
use epics_base_rs::server::records::scalcout::ScalcoutRecord;
use epics_base_rs::server::records::swait::SwaitRecord;
use epics_base_rs::server::records::transform::TransformRecord;
use epics_base_rs::types::EpicsValue;

// ─── H1: longout / int64out drive-limit clamp ────────────────────────

#[test]
fn h1_longout_clamps_val_to_drive_window() {
    let mut r = LongoutRecord::new(0);
    r.drvl = -5;
    r.drvh = 5;
    r.val = 100;
    r.process().unwrap();
    assert_eq!(r.val, 5, "longout VAL above DRVH clamps to DRVH");
    r.val = -100;
    r.process().unwrap();
    assert_eq!(r.val, -5, "longout VAL below DRVL clamps to DRVL");
}

#[test]
fn h1_longout_equal_limits_disable_clamp() {
    let mut r = LongoutRecord::new(0);
    r.drvl = 0;
    r.drvh = 0; // DRVH not > DRVL
    r.val = 9999;
    r.process().unwrap();
    assert_eq!(r.val, 9999, "longout DRVH==DRVL: no clamp (C parity)");
}

#[test]
fn h1_int64out_clamps_val_to_drive_window() {
    let mut r = Int64outRecord::new(0);
    r.drvl = -8.0;
    r.drvh = 8.0;
    r.val = 1_000;
    r.process().unwrap();
    assert_eq!(r.val, 8, "int64out VAL above DRVH clamps to DRVH");
    r.val = -1_000;
    r.process().unwrap();
    assert_eq!(r.val, -8, "int64out VAL below DRVL clamps to DRVL");
}

// ─── M1: calcout ODLY / DLYA output delay ────────────────────────────

#[test]
fn m1_calcout_odly_defers_output_via_reprocess() {
    let mut r = CalcoutRecord::default();
    r.put_field("CALC", EpicsValue::String("A".into())).unwrap();
    r.put_field("A", EpicsValue::Double(42.0)).unwrap();
    r.put_field("ODLY", EpicsValue::Double(0.05)).unwrap();
    // OOPT=0 (Every Time): output should fire.
    let outcome = r.process().unwrap();
    // ODLY > 0: this cycle defers — DLYA set, output suppressed,
    // ReprocessAfter scheduled.
    assert_eq!(
        r.get_field("DLYA"),
        Some(EpicsValue::Short(1)),
        "DLYA set while ODLY delay pending"
    );
    assert!(
        !r.should_output(),
        "ODLY cycle suppresses the OUT-link write"
    );
    assert!(
        outcome
            .actions
            .iter()
            .any(|a| matches!(a, ProcessAction::ReprocessAfter(_))),
        "ODLY cycle schedules a delayed re-process"
    );
    // The delayed re-process: process() runs again, hits the DLYA
    // continuation branch and emits the captured output.
    r.process().unwrap();
    assert_eq!(
        r.get_field("DLYA"),
        Some(EpicsValue::Short(0)),
        "DLYA cleared after delayed re-process"
    );
    assert!(
        r.should_output(),
        "delayed re-process drives the deferred output"
    );
}

#[test]
fn m1_calcout_no_odly_outputs_synchronously() {
    let mut r = CalcoutRecord::default();
    r.put_field("CALC", EpicsValue::String("A".into())).unwrap();
    r.put_field("A", EpicsValue::Double(7.0)).unwrap();
    // ODLY default 0: output is synchronous, no delay.
    let outcome = r.process().unwrap();
    assert_eq!(r.get_field("DLYA"), Some(EpicsValue::Short(0)));
    assert!(r.should_output(), "ODLY=0: output fires this cycle");
    assert!(
        !outcome
            .actions
            .iter()
            .any(|a| matches!(a, ProcessAction::ReprocessAfter(_))),
        "ODLY=0: no delayed re-process scheduled"
    );
}

// ─── M1b: scalcout ODLY / DLYA output delay (sCalcoutRecord.c:399-432) ─

#[test]
fn m1b_scalcout_odly_defers_output_via_reprocess() {
    let mut r = ScalcoutRecord::default();
    r.put_field("CALC", EpicsValue::String("A".into())).unwrap();
    r.put_field("A", EpicsValue::Double(42.0)).unwrap();
    r.put_field("OUT", EpicsValue::String("sink.VAL".into()))
        .unwrap();
    r.put_field("ODLY", EpicsValue::Double(0.05)).unwrap();
    // OOPT=0 (Every Time): output should fire.
    let outcome = r.process().unwrap();
    // ODLY > 0: this cycle defers — DLYA set, OUT write suppressed,
    // ReprocessAfter scheduled. scalcout's should_output() is a recompute
    // helper, so the OUT-write gate is observed through multi_output_links()
    // (which reads cached_should_output).
    assert_eq!(
        r.get_field("DLYA"),
        Some(EpicsValue::Short(1)),
        "DLYA set while ODLY delay pending"
    );
    assert!(
        r.multi_output_links().is_empty(),
        "ODLY cycle suppresses the OUT-link write"
    );
    assert!(
        outcome
            .actions
            .iter()
            .any(|a| matches!(a, ProcessAction::ReprocessAfter(_))),
        "ODLY cycle schedules a delayed re-process"
    );
    // The delayed re-process: process() runs again, hits the DLYA
    // continuation branch and emits the captured output.
    r.process().unwrap();
    assert_eq!(
        r.get_field("DLYA"),
        Some(EpicsValue::Short(0)),
        "DLYA cleared after delayed re-process"
    );
    assert_eq!(
        r.multi_output_links(),
        &[("OUT", "OVAL")],
        "delayed re-process drives the deferred OUT write"
    );
}

#[test]
fn m1b_scalcout_no_odly_outputs_synchronously() {
    let mut r = ScalcoutRecord::default();
    r.put_field("CALC", EpicsValue::String("A".into())).unwrap();
    r.put_field("A", EpicsValue::Double(7.0)).unwrap();
    r.put_field("OUT", EpicsValue::String("sink.VAL".into()))
        .unwrap();
    // ODLY default 0: output is synchronous, no delay.
    let outcome = r.process().unwrap();
    assert_eq!(r.get_field("DLYA"), Some(EpicsValue::Short(0)));
    assert_eq!(
        r.multi_output_links(),
        &[("OUT", "OVAL")],
        "ODLY=0: OUT write fires this cycle"
    );
    assert!(
        !outcome
            .actions
            .iter()
            .any(|a| matches!(a, ProcessAction::ReprocessAfter(_))),
        "ODLY=0: no delayed re-process scheduled"
    );
}

// ─── M1c: swait ODLY output delay (swaitRecord.c:719-729 schedOutput) ─

#[test]
fn m1c_swait_odly_defers_output_via_reprocess() {
    let mut r = SwaitRecord::default();
    r.put_field("CALC", EpicsValue::String("A".into())).unwrap();
    r.put_field("A", EpicsValue::Double(42.0)).unwrap();
    r.put_field("ODLY", EpicsValue::Float(0.05)).unwrap();
    // OOPT=0 (Every Time): output should fire.
    let outcome = r.process().unwrap();
    // ODLY > 0: this cycle defers — output suppressed, ReprocessAfter
    // scheduled. swait's OUT-write gate is should_output() (single
    // parsed_out, no multi_output_links); swait has no DLYA field.
    assert!(
        !r.should_output(),
        "ODLY cycle suppresses the OUT-link write (cached_should_output=false)"
    );
    assert!(
        outcome
            .actions
            .iter()
            .any(|a| matches!(a, ProcessAction::ReprocessAfter(_))),
        "ODLY cycle schedules a delayed re-process"
    );
    // The delayed re-process: process() runs again, hits the output_wait
    // continuation branch and emits the captured output.
    let outcome2 = r.process().unwrap();
    assert!(
        r.should_output(),
        "delayed re-process drives the deferred output"
    );
    assert!(
        !outcome2
            .actions
            .iter()
            .any(|a| matches!(a, ProcessAction::ReprocessAfter(_))),
        "continuation does not re-schedule a delay"
    );
}

#[test]
fn m1c_swait_no_odly_outputs_synchronously() {
    let mut r = SwaitRecord::default();
    r.put_field("CALC", EpicsValue::String("A".into())).unwrap();
    r.put_field("A", EpicsValue::Double(7.0)).unwrap();
    // ODLY default 0: output is synchronous, no delay.
    let outcome = r.process().unwrap();
    assert!(r.should_output(), "ODLY=0: OUT write fires this cycle");
    assert!(
        !outcome
            .actions
            .iter()
            .any(|a| matches!(a, ProcessAction::ReprocessAfter(_))),
        "ODLY=0: no delayed re-process scheduled"
    );
}

// ─── M3: ao IVOA=2 re-converts RVAL from IVOV ────────────────────────

#[test]
fn m3_ao_ivoa_set_to_ivov_reconverts_rval() {
    let mut r = AoRecord::new(0.0);
    r.linr = 1; // SLOPE: RVAL = (VAL - eoff) / eslo
    r.eslo = 2.0;
    r.eoff = 0.0;
    r.ivov = 50.0;
    // IVOA=Set_output_to_IVOV must run the full convert(), so RVAL
    // reflects the converted IVOV — not a stale pre-IVOA value.
    r.apply_invalid_output_value(EpicsValue::Double(50.0))
        .unwrap();
    assert_eq!(r.val, 50.0, "VAL set to IVOV");
    assert_eq!(r.oval, 50.0, "OVAL set to IVOV");
    assert_eq!(r.rval, 25, "RVAL re-converted from IVOV (50/2)");
}

// ─── M5: calc / calcout LA..LU updated only on change ────────────────

#[test]
fn m5_calc_la_advances_only_when_input_changed() {
    let mut r = CalcRecord::new("A+B");
    r.init_record(0).unwrap();
    r.put_field("A", EpicsValue::Double(3.0)).unwrap();
    r.put_field("B", EpicsValue::Double(4.0)).unwrap();
    r.process().unwrap();
    // First process: A changed 0->3, B changed 0->4, so LA=3 LB=4.
    assert_eq!(r.get_field("LA"), Some(EpicsValue::Double(3.0)));
    assert_eq!(r.get_field("LB"), Some(EpicsValue::Double(4.0)));
    // Change only A. B is unchanged, so LB must stay at 4 (its value as
    // of the last post) — not be re-advanced.
    r.put_field("A", EpicsValue::Double(9.0)).unwrap();
    r.process().unwrap();
    assert_eq!(r.get_field("LA"), Some(EpicsValue::Double(9.0)));
    assert_eq!(
        r.get_field("LB"),
        Some(EpicsValue::Double(4.0)),
        "LB unchanged when B did not change"
    );
}

// ─── L1: calc AFVL cleared when AFTC disabled / on UDF ───────────────

#[test]
fn l1_calc_afvl_cleared_when_aftc_disabled() {
    let mut r = CalcRecord::new("1");
    r.init_record(0).unwrap();
    // Simulate a stale accumulator left from a prior AFTC>0 run.
    r.put_field("AFVL", EpicsValue::Double(3.7)).unwrap();
    r.aftc = 0.0; // filter disabled
    r.process().unwrap();
    assert_eq!(
        r.get_field("AFVL"),
        Some(EpicsValue::Double(0.0)),
        "AFVL driven to 0 when AFTC <= 0 (C parity)"
    );
}

#[test]
fn l1_calc_afvl_cleared_on_udf_nan() {
    let mut r = CalcRecord::new("0/0"); // evaluates to NaN
    r.init_record(0).unwrap();
    r.put_field("AFVL", EpicsValue::Double(2.5)).unwrap();
    r.aftc = 10.0; // filter enabled, but VAL is NaN
    r.process().unwrap();
    assert!(
        r.get_field("VAL")
            .and_then(|v| v.to_f64())
            .unwrap()
            .is_nan()
    );
    assert_eq!(
        r.get_field("AFVL"),
        Some(EpicsValue::Double(0.0)),
        "AFVL cleared when VAL is undefined (NaN)"
    );
}

// ─── L3: ai / ao breakpoint-table LINR raises BPT alarm ──────────────

#[test]
fn l3_ai_breakpoint_linr_raises_soft_alarm() {
    let mut r = AiRecord::new(0.0);
    r.linr = 5; // a breakpoint-table menu choice (>= 3)
    r.rval = 100;
    r.process().unwrap();
    let mut common = CommonFields::default();
    r.check_alarms(&mut common);
    assert_eq!(
        common.nsev,
        AlarmSeverity::Major,
        "ai LINR>=3 (breakpoint table) raises MAJOR severity"
    );
}

#[test]
fn l3_ao_breakpoint_linr_raises_soft_alarm() {
    let mut r = AoRecord::new(1.0);
    r.linr = 4; // a breakpoint-table menu choice (>= 3)
    r.process().unwrap();
    let mut common = CommonFields::default();
    r.check_alarms(&mut common);
    assert_eq!(
        common.nsev,
        AlarmSeverity::Major,
        "ao LINR>=3 (breakpoint table) raises MAJOR severity"
    );
}

#[test]
fn l3_ai_linear_linr_no_bpt_alarm() {
    let mut r = AiRecord::new(0.0);
    r.linr = 2; // LINEAR — no breakpoint table
    r.process().unwrap();
    let mut common = CommonFields::default();
    r.check_alarms(&mut common);
    assert_eq!(
        common.nsev,
        AlarmSeverity::NoAlarm,
        "ai LINR=LINEAR raises no BPT alarm"
    );
}

// ─── S1 / S2: scalcout raises CALC_ALARM on broken CALC / OCAL ───────

#[test]
fn s1_scalcout_invalid_calc_raises_calc_alarm() {
    let mut r = ScalcoutRecord::new();
    r.put_field("CALC", EpicsValue::String("A".into())).unwrap();
    r.put_field("A", EpicsValue::Double(5.0)).unwrap();
    r.process().unwrap();
    assert_eq!(
        r.get_field("CALC_ALARM"),
        Some(EpicsValue::Char(0)),
        "valid CALC: no alarm"
    );
    // A failed sCalcPerform must raise CALC_ALARM. Force the calc into a
    // value-stack-underflow state: a bare binary operator with no
    // operands fails at eval time.
    r.put_field("CALC", EpicsValue::String("+".into())).unwrap();
    r.process().unwrap();
    assert_eq!(
        r.get_field("CALC_ALARM"),
        Some(EpicsValue::Char(1)),
        "broken CALC expression raises CALC_ALARM"
    );
}

#[test]
fn s2_scalcout_invalid_ocal_raises_calc_alarm() {
    let mut r = ScalcoutRecord::new();
    r.put_field("CALC", EpicsValue::String("A".into())).unwrap();
    r.put_field("A", EpicsValue::Double(1.0)).unwrap();
    r.put_field("DOPT", EpicsValue::Short(1)).unwrap(); // Use OCAL
    r.put_field("OCAL", EpicsValue::String("+".into())).unwrap();
    r.process().unwrap();
    assert_eq!(
        r.get_field("CALC_ALARM"),
        Some(EpicsValue::Char(1)),
        "broken OCAL expression raises CALC_ALARM"
    );
}

// ─── S3: scalcout drives its OUT link ────────────────────────────────

#[test]
fn s3_scalcout_emits_out_link_when_output_due() {
    let mut r = ScalcoutRecord::new();
    r.put_field("CALC", EpicsValue::String("A".into())).unwrap();
    r.put_field("A", EpicsValue::Double(11.0)).unwrap();
    r.put_field("OUT", EpicsValue::String("target.VAL".into()))
        .unwrap();
    // OOPT=0 Every Time: output is due, so the OUT link is exposed.
    r.process().unwrap();
    assert_eq!(
        r.multi_output_links(),
        &[("OUT", "OVAL")],
        "OUT link written when output is due"
    );
}

#[test]
fn s3_scalcout_suppresses_out_link_when_oopt_not_met() {
    let mut r = ScalcoutRecord::new();
    r.put_field("CALC", EpicsValue::String("A".into())).unwrap();
    r.put_field("A", EpicsValue::Double(0.0)).unwrap();
    r.put_field("OUT", EpicsValue::String("target.VAL".into()))
        .unwrap();
    r.put_field("OOPT", EpicsValue::Short(3)).unwrap(); // When Non-zero
    r.process().unwrap();
    assert!(
        r.multi_output_links().is_empty(),
        "OOPT=When-Nonzero with VAL=0: OUT link suppressed"
    );
}

// ─── S4: transform OUTx write is unconditional (COPT gates calc only) ─
//
// synApps `transformRecord.c` writes every channel with a non-constant
// OUTx every process — its output loop never consults COPT (COPT gates
// only whether CLCx is *evaluated*). The default Conditional mode (COPT=0)
// must therefore still drive the classic `INPx -> A -> OUTx` passthrough
// of an empty-CLCx channel; the prior gate (`write = copt==1 ||
// !calcs[i].is_empty()`) silently dropped it.

#[test]
fn s4_transform_conditional_writes_empty_clc_channel() {
    let mut r = TransformRecord::new();
    r.put_field("COPT", EpicsValue::Short(0)).unwrap(); // Conditional (default)
    // Channel A: a calc + an OUT link. Channel B: NO calc, but an OUT link
    // fed by an external value (the INPx -> field -> OUTx passthrough).
    r.put_field("CLCA", EpicsValue::String("3".into())).unwrap();
    r.put_field("OUTA", EpicsValue::String("a.VAL".into()))
        .unwrap();
    r.put_field("B", EpicsValue::Double(7.0)).unwrap();
    r.put_field("OUTB", EpicsValue::String("b.VAL".into()))
        .unwrap();
    let outcome = r.process().unwrap();
    let writes: Vec<(&str, f64)> = outcome
        .actions
        .iter()
        .filter_map(|a| match a {
            ProcessAction::WriteDbLink {
                link_field,
                value: EpicsValue::Double(v),
            } => Some((*link_field, *v)),
            _ => None,
        })
        .collect();
    assert!(
        writes.contains(&("OUTA", 3.0)),
        "Conditional: channel with CLCx evaluates and writes OUT (=3)"
    );
    assert!(
        writes.contains(&("OUTB", 7.0)),
        "Conditional: empty-CLCx channel still writes its OUT unconditionally \
         (passthrough of B=7) — C transformRecord.c output loop ignores COPT"
    );
}

#[test]
fn s4_transform_always_writes_all_linked_channels() {
    let mut r = TransformRecord::new();
    r.put_field("COPT", EpicsValue::Short(1)).unwrap(); // Always
    r.put_field("OUTB", EpicsValue::String("b.VAL".into()))
        .unwrap();
    let outcome = r.process().unwrap();
    let written: Vec<&str> = outcome
        .actions
        .iter()
        .filter_map(|a| match a {
            ProcessAction::WriteDbLink { link_field, .. } => Some(*link_field),
            _ => None,
        })
        .collect();
    assert!(
        written.contains(&"OUTB"),
        "COPT=Always: channel without CLCx still writes OUT"
    );
}

// COPT gates whether CLCx is EVALUATED. In Conditional mode an
// input-linked channel keeps its INPx value — its CLCx must NOT overwrite
// it (C `transformRecord.c:590` `no_inlink && !new_value`: an input link
// makes `no_inlink` false, so the calc is skipped). In Always mode the
// calc runs and overwrites.
#[test]
fn s4_transform_conditional_input_linked_channel_skips_calc() {
    let mut r = TransformRecord::new();
    r.put_field("COPT", EpicsValue::Short(0)).unwrap(); // Conditional
    // Channel A has an INPx link (so `no_inlink` is false) and a CLCx.
    r.put_field("INPA", EpicsValue::String("src.VAL".into()))
        .unwrap();
    r.put_field("CLCA", EpicsValue::String("A+1".into()))
        .unwrap();
    // Simulate the framework having propagated the input link into A.
    r.put_field("A", EpicsValue::Double(5.0)).unwrap();
    r.process().unwrap();
    assert_eq!(
        r.get_field("A"),
        Some(EpicsValue::Double(5.0)),
        "Conditional + input-linked: CLCx must NOT overwrite the input value"
    );

    // Always mode: the same setup now evaluates CLCA (A+1 = 6).
    r.put_field("COPT", EpicsValue::Short(1)).unwrap();
    r.put_field("A", EpicsValue::Double(5.0)).unwrap();
    r.process().unwrap();
    assert_eq!(
        r.get_field("A"),
        Some(EpicsValue::Double(6.0)),
        "Always: CLCx evaluates regardless of the input link"
    );
}

// ─── S5: transform skips re-computing freshly-put channels ───────────

#[test]
fn s5_transform_fresh_put_survives_one_cycle() {
    let mut r = TransformRecord::new();
    r.put_field("CLCA", EpicsValue::String("99".into()))
        .unwrap();
    // External put to A (channel value field) followed by special().
    r.put_field("A", EpicsValue::Double(7.0)).unwrap();
    r.special("A", true).unwrap();
    r.process().unwrap();
    assert_eq!(
        r.get_field("A"),
        Some(EpicsValue::Double(7.0)),
        "fresh put to transform.A survives this process (not overwritten by CLCA)"
    );
    // Next cycle with no fresh put: CLCA now overwrites.
    r.process().unwrap();
    assert_eq!(
        r.get_field("A"),
        Some(EpicsValue::Double(99.0)),
        "after one cycle CLCA reclaims the channel"
    );
}

// ─── S6: transform IVLA=Do_Nothing is per-channel ────────────────────
//
// S6's divergence (a global abort on one channel's *eval* error) is only
// reachable when a compiled calc fails at eval time. The numeric calc
// compiler's end-of-expression depth check rejects every stack-imbalanced
// expression at *compile* time, so a string-configured CLCx cannot
// produce a runtime eval error — the per-channel-vs-global difference is
// not observable from a `.db`-driven record. The fix is kept as the
// C-correct structure (restore the failing channel only, continue with
// the rest); this test exercises the reachable broken-calc path (a
// compile failure) and confirms it does not abort sibling channels.

#[test]
fn s6_transform_broken_calc_does_not_abort_sibling_channels() {
    let mut r = TransformRecord::new();
    r.put_field("IVLA", EpicsValue::Short(1)).unwrap(); // Do Nothing
    r.put_field("B", EpicsValue::Double(2.0)).unwrap();
    // CLCA is syntactically broken (fails to compile); CLCC is valid.
    r.put_field("CLCA", EpicsValue::String("+".into())).unwrap();
    r.put_field("CLCC", EpicsValue::String("B*5".into()))
        .unwrap();
    r.process().unwrap();
    assert_eq!(
        r.get_field("A"),
        Some(EpicsValue::Double(0.0)),
        "broken CLCA leaves channel A at its prior value"
    );
    assert_eq!(
        r.get_field("C"),
        Some(EpicsValue::Double(10.0)),
        "a broken channel does not abort sibling channel C"
    );
}
