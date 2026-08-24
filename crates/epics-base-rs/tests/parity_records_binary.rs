//! Parity tests for binary / multibit / sel / dfanout records.
//!
//! Covers the findings in `doc/parity-review/07-records-binary.md`:
//! mbbiDirect/mbboDirect 32-bit B0..B1F, mbboDirect RBV,
//! bi/bo/busy/mbbi/mbbo STATE/COS/SOFT alarms,
//! sel SELN range / UDF / SELN update / Median,
//! dfanout limit alarms / deadband.

#![allow(clippy::approx_constant)]

use epics_base_rs::server::recgbl::alarm_status;
use epics_base_rs::server::record::{AlarmSeverity, CommonFields, ProcessAction, Record};
use epics_base_rs::server::records::bi::BiRecord;
use epics_base_rs::server::records::bo::BoRecord;
use epics_base_rs::server::records::dfanout::DfanoutRecord;
use epics_base_rs::server::records::mbbi::MbbiRecord;
use epics_base_rs::server::records::mbbi_direct::MbbiDirectRecord;
use epics_base_rs::server::records::mbbo::MbboRecord;
use epics_base_rs::server::records::mbbo_direct::MbboDirectRecord;
use epics_base_rs::server::records::sel::SelRecord;
use epics_base_rs::types::EpicsValue;

// --- mbbiDirect / mbboDirect span 32 bits B0..B1F ---

#[test]
fn mbbi_direct_exposes_upper_16_bits() {
    let mut rec = MbbiDirectRecord::default();
    // Bit B1A is bit 26 — an INPUT record derives the bit FROM VAL, so drive
    // VAL through process() and read the bit back (a Bx put does NOT fold into
    // VAL here — that is mbboDirect's opposite data flow).
    rec.rval = 1 << 26;
    rec.process().unwrap();
    assert!(matches!(rec.get_field("B1A"), Some(EpicsValue::UChar(1))));
    assert!(matches!(
        rec.get_field("VAL"),
        Some(EpicsValue::Long(v)) if v == (1 << 26)
    ));
}

#[test]
fn mbbi_direct_bit_put_reverts_on_process() {
    // Differential-oracle put-defect #1d: `caput B0 1` on an mbbiDirect whose
    // VAL=0. B0 is pp(TRUE), so the put processes the record. In this INPUT
    // record the bit fields are DERIVED from VAL, and a Bx put is not a value
    // put — VAL stays 0, and C `mbbiDirectRecord.c:217-226` monitor() re-derives
    // B0 = (VAL >> 0) & 1 = 0. The port under-re-derived: it folded the bit into
    // VAL and, on a soft channel (which skips the RVAL->VAL convert), never
    // rebuilt the bits, so `caput B0 1` wrongly ended B0=1.
    let mut rec = MbbiDirectRecord::default();
    rec.put_field("B0", EpicsValue::Char(1)).unwrap();
    assert_eq!(rec.val, 0, "a Bx put must not fold into VAL (mbbiDirect)");
    // Soft-channel process skips the convert (C `read_mbbiDirect` returns 2);
    // the bit rebuild must still run on that path.
    rec.set_device_did_compute(true);
    rec.process().unwrap();
    assert_eq!(rec.bits[0], 0, "process re-derives B0 from VAL=0");
    assert_eq!(rec.val, 0, "VAL unchanged across the bit put + process");
    assert!(matches!(rec.get_field("B0"), Some(EpicsValue::UChar(0))));
}

#[test]
fn mbbi_direct_process_rederives_bits_from_val() {
    // Bit re-derivation across the width: VAL=5 -> B0=1, B2=1, all others 0.
    let mut rec = MbbiDirectRecord::default();
    rec.rval = 5;
    rec.process().unwrap();
    assert_eq!(rec.val, 5);
    assert_eq!(rec.bits[0], 1);
    assert_eq!(rec.bits[1], 0);
    assert_eq!(rec.bits[2], 1);
    assert!(rec.bits[3..].iter().all(|&b| b == 0));
}

#[test]
fn mbbi_direct_process_folds_32_bits() {
    let mut rec = MbbiDirectRecord::default();
    rec.rval = 1 << 31; // top bit
    rec.process().unwrap();
    assert_eq!(rec.val, 1u32 << 31);
    assert_eq!(rec.bits[31], 1);
    assert_eq!(rec.bits[30], 0);
}

#[test]
fn mbbo_direct_exposes_upper_16_bits() {
    let mut rec = MbboDirectRecord::default();
    rec.put_field("B1F", EpicsValue::Char(1)).unwrap();
    assert!(matches!(
        rec.get_field("VAL"),
        Some(EpicsValue::Long(v)) if v as u32 == (1u32 << 31)
    ));
}

#[test]
fn mbbi_direct_nobt_above_16_sets_mask() {
    let mut rec = MbbiDirectRecord::default();
    rec.nobt = 24;
    rec.init_record(0).unwrap();
    // MASK is DBF_ULONG (mbbiDirectRecord.dbd.pod:121).
    assert_eq!(rec.mask, (1u32 << 24) - 1);
}

// --- mbboDirect process must not force RBV = RVAL ---

#[test]
fn mbbo_direct_process_keeps_device_rbv() {
    // Device support wrote a hardware read-back that disagrees with VAL.
    // (Field assignment, not a struct literal: MbboDirectRecord carries a
    // private `value_changed` monitor-gate flag, so it is no longer
    // literal-constructible from outside the crate, matching the other six
    // binary records.)
    let mut rec = MbboDirectRecord::default();
    rec.rbv = 999;
    rec.put_field("VAL", EpicsValue::Long(5)).unwrap();
    rec.process().unwrap();
    assert_eq!(rec.rval, 5, "RVAL is the commanded value");
    assert_eq!(
        rec.rbv, 999,
        "RBV must keep the device read-back, not mirror RVAL"
    );
}

// --- bo STATE / COS alarms ---

#[test]
fn bo_state_alarm_osv() {
    let mut rec = BoRecord::new(1);
    rec.osv = AlarmSeverity::Major as i16;
    // C `boRecord.c::checkAlarms:371-380` raises UDF_ALARM (at UDFS=INVALID)
    // BEFORE the STATE alarm, and `recGblSetSevr` overrides only on strictly
    // greater severity — so on a `udf=1` record STATE never shows. Clear the
    // default UDF to exercise the STATE path (as `bi_state_alarm_zsv` does).
    let mut common = CommonFields {
        udf: 0,
        ..Default::default()
    };
    rec.check_alarms(&mut common);
    assert_eq!(common.nsev, AlarmSeverity::Major);
    assert_eq!(common.nsta, alarm_status::STATE_ALARM);
}

#[test]
fn bo_high_one_shot_resets_val() {
    let mut rec = BoRecord::new(1);
    rec.high = 0.5;
    let outcome = rec.process().unwrap();
    assert!(
        outcome
            .actions
            .iter()
            .any(|a| matches!(a, ProcessAction::DelayedCallbackAfter(_))),
        "HIGH>0 with VAL=1 arms C's `callbackRequestDelayed` (boRecord.c:257-262)"
    );
    // The timer body, not a process cycle, is what drives VAL back to Done —
    // `boRecord.c::myCallbackFunc:116` is the only writer of the one-shot's
    // `prec->val = 0`. A plain reprocess must leave the pulse standing.
    rec.process().unwrap();
    assert_eq!(rec.val, 1, "a process cycle cannot consume the one-shot");
    rec.delayed_callback_fire(false);
    assert_eq!(rec.val, 0, "momentary bo returns to Done after HIGH");
}

// --- bi STATE alarm ---

#[test]
fn bi_state_alarm_zsv() {
    let mut rec = BiRecord::new(0);
    rec.zsv = AlarmSeverity::Minor as i16;
    // C `biRecord.c::checkAlarms:232-235` raises UDF_ALARM and
    // *returns* when `udf` is set — STATE is only reached on a
    // defined record. `CommonFields::default()` has `udf=true`
    // (uninitialised), so clear it to exercise the STATE path,
    // mirroring what `process_local` does via `value_is_undefined()`.
    let mut common = CommonFields {
        udf: 0,
        ..Default::default()
    };
    rec.check_alarms(&mut common);
    assert_eq!(common.nsev, AlarmSeverity::Minor);
    assert_eq!(common.nsta, alarm_status::STATE_ALARM);
}

#[test]
fn bi_cos_alarm_fires_on_change() {
    let mut rec = BiRecord::new(1);
    rec.cosv = AlarmSeverity::Major as i16;
    rec.lalm = 0; // previous value differs
    // See bi_state_alarm_zsv: clear the default UDF so checkAlarms
    // evaluates COS instead of returning after UDF_ALARM.
    let mut common = CommonFields {
        udf: 0,
        ..Default::default()
    };
    rec.check_alarms(&mut common);
    assert_eq!(common.nsev, AlarmSeverity::Major);
    assert_eq!(common.nsta, alarm_status::COS_ALARM);
    // Second evaluation with no change: COS does not re-fire.
    let mut common2 = CommonFields {
        udf: 0,
        ..Default::default()
    };
    rec.check_alarms(&mut common2);
    assert_eq!(common2.nsev, AlarmSeverity::NoAlarm);
}

// --- mbbi STATE alarm from per-state severity ---

#[test]
fn mbbi_state_alarm_per_state() {
    let mut rec = MbbiRecord::new(2);
    rec.twsv = AlarmSeverity::Major as i16; // state 2 severity
    // C `mbbiRecord.c::checkAlarms:300-305` raises UDF_ALARM and
    // returns when `udf` is set; clear the default UDF to exercise
    // the per-state STATE alarm (process_local clears it via
    // `value_is_undefined()` for a defined VAL).
    let mut common = CommonFields {
        udf: 0,
        ..Default::default()
    };
    rec.check_alarms(&mut common);
    assert_eq!(common.nsev, AlarmSeverity::Major);
    assert_eq!(common.nsta, alarm_status::STATE_ALARM);
}

// --- mbbo STATE alarm + SOFT_ALARM on illegal VAL ---

#[test]
fn mbbo_soft_alarm_on_illegal_val() {
    let mut rec = MbboRecord::new(20); // > 15
    rec.zrvl = 1; // define a state table → sdef=true
    rec.init_record(0).unwrap();
    rec.process().unwrap();
    // C mbbo raises UDF in `process()` before `checkAlarms`, and its SOFT alarm
    // is raised by `convert()` — which the `udf` early-exit (`goto CONTINUE`)
    // SKIPS. So SOFT is a defined-record alarm; clear the default UDF to isolate
    // it (as `bo_state_alarm_osv`/`bi_state_alarm_zsv` do).
    let mut common = CommonFields {
        udf: 0,
        ..Default::default()
    };
    rec.check_alarms(&mut common);
    assert_eq!(common.nsev, AlarmSeverity::Invalid);
    assert_eq!(common.nsta, alarm_status::SOFT_ALARM);
}

#[test]
fn mbbo_state_alarm_per_state() {
    let mut rec = MbboRecord::new(3);
    rec.thsv = AlarmSeverity::Minor as i16;
    // mbbo raises UDF (at UDFS=INVALID) before the STATE alarm; a `udf=1`
    // record would show UDF, not the Minor STATE. Clear the default UDF to
    // isolate the per-state severity (as `bo_state_alarm_osv` does).
    let mut common = CommonFields {
        udf: 0,
        ..Default::default()
    };
    rec.check_alarms(&mut common);
    assert_eq!(common.nsev, AlarmSeverity::Minor);
    assert_eq!(common.nsta, alarm_status::STATE_ALARM);
}

// --- sel SELN range check + UDF propagation ---

#[test]
fn sel_specified_out_of_range_raises_soft_alarm() {
    let mut rec = SelRecord::default();
    rec.selm = 0; // Specified
    rec.seln = 50; // >= SEL_MAX (12)
    rec.val = 7.0; // stale value
    rec.process().unwrap();
    assert_eq!(rec.val, 7.0, "VAL unchanged on out-of-range SELN");
    let mut common = CommonFields::default();
    rec.check_alarms(&mut common);
    assert_eq!(common.nsev, AlarmSeverity::Invalid);
    assert_eq!(common.nsta, alarm_status::SOFT_ALARM);
}

#[test]
fn sel_specified_negative_seln_treated_out_of_range() {
    let mut rec = SelRecord::default();
    rec.selm = 0;
    rec.seln = 0xFFFF; // a client -1 casts to DBF_USHORT 65535 → out of range
    rec.process().unwrap();
    let mut common = CommonFields::default();
    rec.check_alarms(&mut common);
    assert_eq!(common.nsev, AlarmSeverity::Invalid);
    assert_eq!(common.nsta, alarm_status::SOFT_ALARM);
}

#[test]
fn sel_specified_selects_input_and_nan_marks_undefined() {
    let mut rec = SelRecord::default();
    rec.selm = 0;
    rec.seln = 1; // B
    rec.b = 3.5;
    rec.process().unwrap();
    assert_eq!(rec.val, 3.5);
    assert!(!rec.value_is_undefined());

    // Select an input that is still NaN — VAL becomes NaN → undefined.
    rec.seln = 2; // C is NaN by default
    rec.process().unwrap();
    assert!(rec.val.is_nan());
    assert!(rec.value_is_undefined());
}

// --- sel High/Low/Median update SELN ---

#[test]
fn sel_high_signal_updates_seln() {
    let mut rec = SelRecord::default();
    rec.selm = 1; // High
    rec.a = 1.0;
    rec.b = 9.0;
    rec.c = 4.0;
    rec.process().unwrap();
    assert_eq!(rec.val, 9.0);
    assert_eq!(rec.seln, 1, "SELN reflects winning index (B)");
}

#[test]
fn sel_low_signal_updates_seln() {
    let mut rec = SelRecord::default();
    rec.selm = 2; // Low
    rec.a = 5.0;
    rec.b = 2.0;
    rec.c = 8.0;
    rec.process().unwrap();
    assert_eq!(rec.val, 2.0);
    assert_eq!(rec.seln, 1);
}

#[test]
fn sel_median_sets_seln_to_count() {
    let mut rec = SelRecord::default();
    rec.selm = 3; // Median
    rec.a = 1.0;
    rec.b = 5.0;
    rec.c = 3.0;
    rec.process().unwrap();
    assert_eq!(rec.val, 3.0, "median of [1,3,5]");
    assert_eq!(rec.seln, 3, "SELN = number of valid inputs");
}

// --- sel Median with no valid inputs → NaN / undefined ---

#[test]
fn sel_median_empty_yields_undefined() {
    let mut rec = SelRecord::default();
    rec.selm = 3;
    // All inputs default to NaN.
    rec.process().unwrap();
    assert!(rec.val.is_nan());
    assert_eq!(rec.seln, 0);
    assert!(rec.value_is_undefined());
}

// --- sel High/Low with no valid inputs → ±inf, UDF clear (C parity) ---

#[test]
fn sel_high_empty_yields_neg_inf_udf_clear() {
    let mut rec = SelRecord::default();
    rec.selm = 1; // High
    // C `selRecord.c:362` seeds `val = -epicsINF`; when every input is
    // NaN the loop never updates `val`, so `prec->val = val` assigns
    // -inf. `prec->udf = isnan(prec->val)` (selRecord.c:402) — and
    // `isnan(-inf)` is FALSE — so UDF stays CLEAR. (Contrast SELM=3
    // Median, whose `order[0] = epicsNAN` seed genuinely yields NaN.)
    rec.process().unwrap();
    assert!(
        rec.val == f64::NEG_INFINITY,
        "all-NaN High selection must yield VAL = -inf (C selRecord.c:362), got {}",
        rec.val
    );
    assert!(
        !rec.value_is_undefined(),
        "all-NaN High: isnan(-inf) is FALSE so UDF must stay clear"
    );
}

#[test]
fn sel_low_empty_yields_pos_inf_udf_clear() {
    let mut rec = SelRecord::default();
    rec.selm = 2; // Low
    // C `selRecord.c:371` seeds `val = epicsINF`; an all-NaN Low
    // selection leaves VAL = +inf with UDF CLEAR (isnan(+inf) FALSE).
    rec.process().unwrap();
    assert!(
        rec.val == f64::INFINITY,
        "all-NaN Low selection must yield VAL = +inf (C selRecord.c:371), got {}",
        rec.val
    );
    assert!(
        !rec.value_is_undefined(),
        "all-NaN Low: isnan(+inf) is FALSE so UDF must stay clear"
    );
}

// --- sel limit alarms ---

#[test]
fn sel_high_limit_alarm() {
    let mut rec = SelRecord::default();
    rec.selm = 0;
    rec.seln = 0;
    rec.a = 100.0;
    rec.high = 50.0;
    rec.hsv = AlarmSeverity::Minor as i16;
    rec.process().unwrap();
    assert_eq!(rec.val, 100.0);
    // `process_local` derives `common.udf` from `value_is_undefined()`
    // before `check_alarms` (C `selRecord.c::do_sel` sets
    // `udf = isnan(val)`). `CommonFields::default()` is `udf=true`;
    // mirror the framework so the limit alarm is reached instead of
    // UDF_ALARM (C `selRecord.c::checkAlarms:256-259`).
    let mut common = CommonFields {
        udf: rec.value_is_undefined() as u8,
        ..Default::default()
    };
    rec.check_alarms(&mut common);
    assert_eq!(common.nsev, AlarmSeverity::Minor);
    assert_eq!(common.nsta, alarm_status::HIGH_ALARM);
}

#[test]
fn sel_hihi_limit_alarm_takes_priority() {
    let mut rec = SelRecord::default();
    rec.selm = 0;
    rec.seln = 0;
    rec.a = 100.0;
    rec.high = 50.0;
    rec.hsv = AlarmSeverity::Minor as i16;
    rec.hihi = 90.0;
    rec.hhsv = AlarmSeverity::Major as i16;
    rec.process().unwrap();
    // See sel_high_limit_alarm: mirror the framework UDF wiring.
    let mut common = CommonFields {
        udf: rec.value_is_undefined() as u8,
        ..Default::default()
    };
    rec.check_alarms(&mut common);
    assert_eq!(common.nsev, AlarmSeverity::Major);
    assert_eq!(common.nsta, alarm_status::HIHI_ALARM);
}

// --- dfanout limit alarms ---

#[test]
fn dfanout_low_limit_alarm() {
    let mut rec = DfanoutRecord::new(-10.0);
    rec.low = 0.0;
    rec.lsv = AlarmSeverity::Minor as i16;
    rec.process().unwrap();
    // `process_local` derives `common.udf` from `value_is_undefined()`
    // before `check_alarms` (C `dfanoutRecord.c` sets `udf=isnan(val)`).
    // `CommonFields::default()` is `udf=true`; mirror the framework so
    // the limit alarm is reached instead of UDF_ALARM
    // (C `dfanoutRecord.c::checkAlarms:233-236`).
    let mut common = CommonFields {
        udf: rec.value_is_undefined() as u8,
        ..Default::default()
    };
    rec.check_alarms(&mut common);
    assert_eq!(common.nsev, AlarmSeverity::Minor);
    assert_eq!(common.nsta, alarm_status::LOW_ALARM);
}

#[test]
fn dfanout_lolo_limit_alarm() {
    let mut rec = DfanoutRecord::new(-100.0);
    rec.low = 0.0;
    rec.lsv = AlarmSeverity::Minor as i16;
    rec.lolo = -50.0;
    rec.llsv = AlarmSeverity::Major as i16;
    rec.process().unwrap();
    // See dfanout_low_limit_alarm: mirror the framework UDF wiring.
    let mut common = CommonFields {
        udf: rec.value_is_undefined() as u8,
        ..Default::default()
    };
    rec.check_alarms(&mut common);
    assert_eq!(common.nsev, AlarmSeverity::Major);
    assert_eq!(common.nsta, alarm_status::LOLO_ALARM);
}

#[test]
fn dfanout_no_alarm_when_in_range() {
    let mut rec = DfanoutRecord::new(5.0);
    rec.high = 50.0;
    rec.hsv = AlarmSeverity::Major as i16;
    rec.low = -50.0;
    rec.lsv = AlarmSeverity::Major as i16;
    rec.process().unwrap();
    // See dfanout_low_limit_alarm: mirror the framework UDF wiring.
    let mut common = CommonFields {
        udf: rec.value_is_undefined() as u8,
        ..Default::default()
    };
    rec.check_alarms(&mut common);
    assert_eq!(common.nsev, AlarmSeverity::NoAlarm);
}

#[test]
fn dfanout_udf_alarm_on_nan_val() {
    let mut rec = DfanoutRecord::new(f64::NAN);
    assert!(rec.value_is_undefined());
    // framework sets this from value_is_undefined()
    let mut common = CommonFields {
        udf: 1,
        ..Default::default()
    };
    rec.check_alarms(&mut common);
    assert_eq!(common.nsev, AlarmSeverity::from_u16(common.udfs as u16));
    assert_eq!(common.nsta, alarm_status::UDF_ALARM);
}

#[test]
fn dfanout_output_links_preserved() {
    let mut rec = DfanoutRecord::default();
    rec.put_field("OUTA", EpicsValue::String("REC_A".into()))
        .unwrap();
    rec.put_field("OUTP", EpicsValue::String("REC_P".into()))
        .unwrap();
    let links = rec.output_links();
    assert_eq!(links, vec!["REC_A", "REC_P"]);
}

// --- dfanout MDEL/ADEL fields exposed for framework deadband ---

#[test]
fn dfanout_exposes_deadband_fields() {
    let mut rec = DfanoutRecord::default();
    rec.put_field("MDEL", EpicsValue::Double(2.0)).unwrap();
    rec.put_field("ADEL", EpicsValue::Double(5.0)).unwrap();
    assert!(matches!(
        rec.get_field("MDEL"),
        Some(EpicsValue::Double(v)) if (v - 2.0).abs() < 1e-9
    ));
    assert!(matches!(
        rec.get_field("ADEL"),
        Some(EpicsValue::Double(v)) if (v - 5.0).abs() < 1e-9
    ));
    assert!(rec.uses_monitor_deadband());
}
