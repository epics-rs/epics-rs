#![allow(unused_imports, clippy::all)]
use epics_base_rs::error::{CaError, CaResult};
use epics_base_rs::server::record::*;
use epics_base_rs::server::records::ai::AiRecord;
use epics_base_rs::server::records::ao::AoRecord;
use epics_base_rs::server::records::bi::BiRecord;
use epics_base_rs::server::records::stringin::StringinRecord;
use epics_base_rs::types::{DbFieldType, EpicsValue, PvString};

#[test]
fn test_ai_record_type() {
    let rec = AiRecord::new(25.0);
    assert_eq!(rec.record_type(), "ai");
}

#[test]
fn test_ai_get_val() {
    let rec = AiRecord::new(42.0);
    match rec.get_field("VAL") {
        Some(EpicsValue::Double(v)) => assert!((v - 42.0).abs() < 1e-10),
        other => panic!("expected Double(42.0), got {:?}", other),
    }
}

#[test]
fn test_ai_put_val() {
    let mut rec = AiRecord::new(0.0);
    rec.put_field("VAL", EpicsValue::Double(99.0)).unwrap();
    match rec.get_field("VAL") {
        Some(EpicsValue::Double(v)) => assert!((v - 99.0).abs() < 1e-10),
        other => panic!("expected Double(99.0), got {:?}", other),
    }
}

#[test]
fn test_ai_string_field() {
    let mut rec = AiRecord::default();
    rec.put_field("EGU", EpicsValue::String("celsius".into()))
        .unwrap();
    match rec.get_field("EGU") {
        Some(EpicsValue::String(s)) => assert_eq!(s, "celsius"),
        other => panic!("expected String, got {:?}", other),
    }
}

#[test]
fn test_ai_field_list() {
    let rec = AiRecord::default();
    let fields = rec.field_list();

    // The table is generated from `aiRecord.dbd`, so it is in the spec's
    // declaration order (C's field order) and carries every field the record
    // type declares — including the ones the framework, not the record, drives.
    assert_eq!(fields[0].name, "VAL");
    assert_eq!(fields[0].dbf_type, DbFieldType::Double);
    for declared in ["INP", "EGU", "PREC", "LINR", "SIMM", "SIML", "SIOL"] {
        assert!(
            fields.iter().any(|f| f.name == declared),
            "ai.{declared} is in aiRecord.dbd but not in field_list()"
        );
    }

    // LINR is `DBF_MENU menu(menuConvert)`: served as DBR_ENUM *with* its
    // choices, which is why `caget ai.LINR` on the C IOC answers
    // "NO CONVERSION" and not "0".
    let linr = fields.iter().find(|f| f.name == "LINR").unwrap();
    assert_eq!(linr.dbf_type, DbFieldType::Enum);
    assert_eq!(linr.menu.unwrap()[0], "NO CONVERSION");
}

#[test]
fn test_ai_unknown_field() {
    let rec = AiRecord::default();
    assert!(rec.get_field("NONEXISTENT").is_none());
}

#[test]
fn test_ai_put_type_mismatch() {
    let mut rec = AiRecord::default();
    let result = rec.put_field("VAL", EpicsValue::String("bad".into()));
    assert!(result.is_err());
}

#[test]
fn test_ai_put_unknown_field() {
    let mut rec = AiRecord::default();
    let result = rec.put_field("NONEXISTENT", EpicsValue::Double(1.0));
    assert!(result.is_err());
}

#[test]
fn test_ao_record() {
    let mut rec = AoRecord::new(10.0);
    assert_eq!(rec.record_type(), "ao");
    rec.put_field("VAL", EpicsValue::Double(20.0)).unwrap();
    match rec.get_field("VAL") {
        Some(EpicsValue::Double(v)) => assert!((v - 20.0).abs() < 1e-10),
        other => panic!("expected Double(20.0), got {:?}", other),
    }
}

// asyn output readback (devAsynInt32.c processAo/initAo): an asynInt32 output
// record reads the device's current value back through the `raw → eng` INVERSE
// of convert(). AoRecord::apply_raw_readback stores the raw to RVAL and
// computes VAL; the skip_convert gate stops process()'s forward convert from
// overwriting the readback.
#[test]
fn test_ao_readback_inverts_full_conversion_chain() {
    // C processAo readback (devAsynInt32.c:973-994) is convert() inverted in
    // reverse field order — un-ROFF, un-ASLO/AOFF, then ESLO/EOFF.
    // raw=10: (10+1)*2 + 3 = 25; LINEAR: 25*5 + 7 = 132.
    let mut rec = AoRecord::new(0.0);
    rec.roff = 1;
    rec.aslo = 2.0;
    rec.aoff = 3.0;
    rec.linr = 2; // LINEAR
    rec.eslo = 5.0;
    rec.eoff = 7.0;
    assert!(
        rec.apply_raw_readback(10),
        "ao owns the raw->eng readback convert"
    );
    assert_eq!(rec.rval, 10, "raw value stored to RVAL");
    match rec.get_field("VAL") {
        Some(EpicsValue::Double(v)) => assert!((v - 132.0).abs() < 1e-9, "got {v}"),
        other => panic!("expected Double(132.0), got {other:?}"),
    }
}

#[test]
fn test_ao_readback_no_conversion_passthrough() {
    // LINR=NO_CONVERSION, default ASLO=1/AOFF=0/ROFF=0 → VAL == raw.
    let mut rec = AoRecord::new(0.0);
    rec.linr = 0;
    assert!(rec.apply_raw_readback(42));
    assert_eq!(rec.rval, 42);
    match rec.get_field("VAL") {
        Some(EpicsValue::Double(v)) => assert!((v - 42.0).abs() < 1e-9, "got {v}"),
        other => panic!("expected Double(42.0), got {other:?}"),
    }
}

#[test]
fn test_ao_readback_skip_convert_bypasses_forward_convert() {
    // C `processAo` returns from its readback branch WITHOUT calling convertAo
    // (devAsynInt32.c:970-994): the readback applies neither drive limits nor
    // OROC. The skip_convert gate (set via set_device_did_compute) mirrors that
    // — process() must not run the forward convert that would clamp VAL.
    let mut rec = AoRecord::new(0.0);
    rec.linr = 2; // LINEAR
    rec.eslo = 1.0;
    rec.eoff = 0.0;
    rec.drvl = 0.0;
    rec.drvh = 100.0; // a forward convert would clamp VAL to this
    assert!(rec.apply_raw_readback(150)); // eng VAL = 150 (eslo=1, eoff=0)
    assert_eq!(rec.rval, 150);
    assert!((rec.val - 150.0).abs() < 1e-9);

    // did_compute → skip the forward convert this cycle.
    rec.set_device_did_compute(true);
    let _ = rec.process().unwrap();
    assert!(
        (rec.val - 150.0).abs() < 1e-9,
        "readback VAL must not be drive-clamped by a skipped forward convert"
    );
    assert_eq!(rec.rval, 150, "readback RVAL preserved");

    // One-shot: the next normal process runs the forward convert, which DOES
    // drive-clamp VAL (150 → 100) — proving the gate was what bypassed it.
    let _ = rec.process().unwrap();
    assert!(
        (rec.val - 100.0).abs() < 1e-9,
        "forward convert now clamps VAL to DRVH"
    );
}

// The asynFloat64 ao readback hook applies the forward ASLO/AOFF scaling
// (VAL = value*ASLO + AOFF) and sets VAL only — a float64 ao carries no raw
// path, so RVAL is untouched. C `initAo`/`processAo` (devAsynFloat64.c:628-630
// / :647-649). Contrast apply_raw_readback (int32), which also seeds RVAL.
#[test]
fn test_ao_float64_readback_applies_aslo_aoff_val_only() {
    let mut rec = AoRecord::new(0.0);
    rec.aslo = 2.0;
    rec.aoff = 1.0;
    rec.rval = 999; // sentinel: the float64 readback must not touch RVAL
    assert!(rec.apply_float64_readback(10.0));
    match rec.get_field("VAL") {
        Some(EpicsValue::Double(v)) => assert!(
            (v - 21.0).abs() < 1e-9,
            "VAL = raw*ASLO + AOFF = 10*2 + 1 = 21, got {v}"
        ),
        other => panic!("expected Double(21.0), got {other:?}"),
    }
    assert_eq!(rec.rval, 999, "float64 ao readback leaves RVAL untouched");
}

// The default apply_float64_readback declines (returns false): a non-float64
// output (e.g. mbbo) keeps the int32 raw path and must not be re-routed here.
#[test]
fn test_mbbo_declines_float64_readback() {
    use epics_base_rs::server::records::mbbo::MbboRecord;
    let mut rec = MbboRecord::new(0);
    assert!(
        !rec.apply_float64_readback(3.0),
        "mbbo declines the float64 readback hook (keeps the int32 raw path)"
    );
}

// The default apply_raw_readback returns false: an input record (ai) whose own
// convert() is already raw->eng must NOT claim to handle the readback, or the
// asyn store would skip the framework's RVAL->VAL convert that ai relies on.
#[test]
fn test_ai_does_not_claim_raw_readback() {
    let mut rec = AiRecord::new(0.0);
    assert!(
        !rec.apply_raw_readback(5),
        "ai keeps the legacy raw->RVAL + framework-convert path"
    );
}

// C processMbbo readback (devAsynInt32.c:1311-1330 / devAsynUInt32Digital.c
// :945-962): rval = value & mask; if shft>0 rval>>=shft; val = state index of
// the shifted raw. RVAL keeps the masked (unshifted) raw.
#[test]
fn test_mbbo_readback_maps_raw_to_state_index() {
    use epics_base_rs::server::records::mbbo::MbboRecord;
    let mut rec = MbboRecord::new(0);
    // State table ZRVL=0, ONVL=1, TWVL=2 → defined states 0/1/2.
    rec.put_field("ONVL", EpicsValue::ULong(1)).unwrap();
    rec.put_field("TWVL", EpicsValue::ULong(2)).unwrap();
    rec.init_record(0).unwrap(); // computes sdef=true
    // Set MASK/SHFT after init so the nobt-derived init mask doesn't clobber.
    rec.mask = 0x0C; // bits 2-3
    rec.shft = 2;
    // raw 0x08 → masked 0x08 → shifted (>>2) = 2 → TWVL=2 → state index 2.
    assert!(
        rec.apply_raw_readback(0x08),
        "mbbo owns the readback state map"
    );
    assert_eq!(rec.rval, 0x08, "RVAL keeps the masked (unshifted) raw");
    assert_eq!(rec.val, 2, "shifted raw 2 → state index 2 (TWVL)");
}

// No state matches the shifted raw → VAL = 65535 (C "unknown state"), the
// reverse of mbbi::raw_to_val.
#[test]
fn test_mbbo_readback_unknown_state_is_65535() {
    use epics_base_rs::server::records::mbbo::MbboRecord;
    let mut rec = MbboRecord::new(0);
    rec.put_field("ONVL", EpicsValue::ULong(1)).unwrap();
    rec.init_record(0).unwrap();
    rec.mask = 0x0C;
    rec.shft = 2;
    // raw 0x0C → masked 0x0C → shifted 3; no state has raw value 3.
    assert!(rec.apply_raw_readback(0x0C));
    assert_eq!(rec.rval, 0x0C);
    assert_eq!(rec.val, 65535, "no matching state → unknown sentinel");
}

// C processBo readback: val = (rval != 0). asynUInt32Digital masks
// (rval = value & mask, :731-732); asynInt32 does not (rval = value,
// :1202-1203). The `mask != 0` split reproduces both: mask set → masked,
// mask 0 → raw.
#[test]
fn test_bo_readback_maps_raw_to_binary_both_mask_modes() {
    use epics_base_rs::server::records::bo::BoRecord;
    // mask != 0 (digital): rval = raw & mask, val = (rval != 0).
    let mut rec = BoRecord::new(0);
    rec.mask = 0x04;
    assert!(rec.apply_raw_readback(0x04));
    assert_eq!(rec.rval, 0x04);
    assert_eq!(rec.val, 1);
    // Out-of-mask bits only → masked 0 → val 0.
    assert!(rec.apply_raw_readback(0x02));
    assert_eq!(rec.rval, 0);
    assert_eq!(rec.val, 0);
    // mask == 0 (asynInt32 bo): rval = raw (unmasked), val = (raw != 0).
    let mut rec2 = BoRecord::new(0);
    rec2.mask = 0;
    assert!(rec2.apply_raw_readback(0x02));
    assert_eq!(
        rec2.rval, 0x02,
        "asynInt32 bo keeps the unmasked raw in RVAL"
    );
    assert_eq!(rec2.val, 1);
}

// The skip_convert gate (set via set_device_did_compute) makes process() skip
// the forward convert that would recompute RVAL from VAL and discard the
// readback — C processBo returns from its readback branch without converting.
#[test]
fn test_bo_readback_skip_convert_preserves_rval() {
    use epics_base_rs::server::records::bo::BoRecord;
    let mut rec = BoRecord::new(0);
    rec.mask = 0; // asynInt32 bo: RVAL holds the raw, not just 0/1
    assert!(rec.apply_raw_readback(0x2A)); // rval=0x2A, val=1
    assert_eq!(rec.rval, 0x2A);
    assert_eq!(rec.val, 1);
    rec.set_device_did_compute(true);
    let _ = rec.process().unwrap();
    assert_eq!(rec.rval, 0x2A, "skip_convert preserves the readback RVAL");
    // One-shot: the next normal process runs val_to_rval (mask=0 → rval=val=1),
    // proving the gate is what bypassed it.
    let _ = rec.process().unwrap();
    assert_eq!(
        rec.rval, 1,
        "forward convert recomputes rval from val (mask=0)"
    );
}

// C processMbboDirect readback (devAsynUInt32Digital.c:1084-1090): rval =
// value & mask; if shft>0 rval>>=shft; val = rval (no state table). The
// skip_convert gate preserves the readback RVAL including bits below SHFT that
// the forward convert ((val<<shft)&mask) would truncate.
#[test]
fn test_mbbo_direct_readback_maps_raw_and_skip_convert_preserves_low_bits() {
    use epics_base_rs::server::records::mbbo_direct::MbboDirectRecord;
    let mut rec = MbboDirectRecord::default();
    rec.mask = 0x0F;
    rec.shft = 2;
    // raw 0x0F → masked 0x0F → val = 0x0F >> 2 = 3; bits 0,1 set.
    assert!(rec.apply_raw_readback(0x0F));
    assert_eq!(rec.rval, 0x0F, "RVAL keeps the masked (unshifted) raw");
    assert_eq!(rec.val, 3, "VAL = masked raw >> SHFT");
    assert_eq!(rec.bits[0], 1);
    assert_eq!(rec.bits[1], 1);
    rec.set_device_did_compute(true);
    let _ = rec.process().unwrap();
    assert_eq!(
        rec.rval, 0x0F,
        "skip_convert preserves the readback RVAL with its sub-SHFT bits"
    );
    // One-shot: the next normal process forward-converts, which truncates the
    // low bits: (3 << 2) & 0x0F = 0x0C — proving the gate mattered.
    let _ = rec.process().unwrap();
    assert_eq!(
        rec.rval, 0x0C,
        "forward convert loses the sub-SHFT bits the readback preserved"
    );
}

// INPUT twin of the bo readback. C processBi sets rval then biRecord converts
// val = (rval != 0): asynInt32 processBi does NOT mask (rval = value, initBi
// passes a NULL mask → mask 0); asynUInt32Digital processBi masks (rval =
// value & mask, devAsynUInt32Digital.c:689). The `mask != 0` split reproduces
// both. Before this fix the device raw fell to the default set_val and landed
// the raw count in VAL (e.g. 0x80 → val=128) instead of the 0/1 bi exposes.
#[test]
fn test_bi_readback_maps_raw_to_binary_both_mask_modes() {
    // asynInt32 bi (mask == 0): rval = raw, val = (raw != 0). A non-binary raw
    // resolves to 1, it is NOT stored verbatim.
    let mut rec = BiRecord::new(0);
    rec.mask = 0;
    assert!(rec.apply_raw_readback(5), "bi claims the device readback");
    assert_eq!(rec.rval, 5, "asynInt32 bi keeps the unmasked raw in RVAL");
    assert_eq!(rec.val, 1, "VAL is 0/1, not the raw count");
    assert!(rec.apply_raw_readback(0));
    assert_eq!(rec.val, 0);
    // asynUInt32Digital bi (mask != 0): rval = raw & mask, val = (rval != 0).
    let mut rec2 = BiRecord::new(0);
    rec2.mask = 0x80;
    assert!(rec2.apply_raw_readback(0x80));
    assert_eq!(rec2.rval, 0x80);
    assert_eq!(
        rec2.val, 1,
        "high-bit mask hit → val 1 (C gives 1, not 128)"
    );
    // A raw whose set bits are all outside the mask → masked 0 → val 0.
    assert!(rec2.apply_raw_readback(0x01));
    assert_eq!(rec2.rval, 0);
    assert_eq!(rec2.val, 0, "bits outside MASK do not set VAL");
}

// INPUT twin of the mbboDirect readback (asynUInt32Digital only). C
// processMbbiDirect sets rval = value & mask (devAsynUInt32Digital.c:1031);
// mbbiDirectRecord convert resolves val = (masked >> SHFT) and the bit fields.
// Before this fix the raw fell to the default set_val → VAL = raw verbatim (no
// MASK, no SHFT, wrong bits).
#[test]
fn test_mbbi_direct_readback_maps_raw_mask_shift_bits() {
    use epics_base_rs::server::records::mbbi_direct::MbbiDirectRecord;
    let mut rec = MbbiDirectRecord::default();
    rec.mask = 0x3C; // bits 2-5
    rec.shft = 2;
    // raw 0x3C → masked 0x3C → val = 0x3C >> 2 = 0x0F; bits 0-3 set.
    assert!(
        rec.apply_raw_readback(0x3C),
        "mbbiDirect claims the readback"
    );
    assert_eq!(rec.rval, 0x3C, "RVAL keeps the masked (unshifted) raw");
    assert_eq!(rec.val, 0x0F, "VAL = masked raw >> SHFT");
    assert_eq!(rec.bits[0], 1);
    assert_eq!(rec.bits[3], 1);
    assert_eq!(rec.bits[4], 0);
    // The skip_convert gate preserves the readback RVAL (incl. sub-SHFT bits a
    // forward convert would truncate) — C processMbbiDirect returns 0 from its
    // readback without re-converting.
    rec.set_device_did_compute(true);
    let _ = rec.process().unwrap();
    assert_eq!(rec.rval, 0x3C, "skip_convert preserves the readback RVAL");
    assert_eq!(rec.val, 0x0F);
    // Bits outside MASK are dropped: raw 0xC0 (bits 6-7) & 0x3C = 0 → val 0.
    assert!(rec.apply_raw_readback(0xC0));
    assert_eq!(rec.rval, 0);
    assert_eq!(rec.val, 0, "out-of-mask bits do not reach VAL");
}

// Family boundary: longin's VAL *is* the raw (C processLi sets pr->val, no
// RVAL->VAL convert), so it declines apply_raw_readback and the asyn store
// keeps routing it through set_val (the default hook returns false).
#[test]
fn test_longin_declines_raw_readback() {
    use epics_base_rs::server::records::longin::LonginRecord;
    assert!(
        !LonginRecord::new(0).apply_raw_readback(3),
        "longin VAL is the raw — no convert to claim"
    );
}

// mbbi asyn device readback (input twin of mbbo): C processMbbi masks on both
// ifaces (rval = value & mask, devAsynInt32.c:1270 / devAsynUInt32Digital.c:903)
// then mbbiRecord convert shifts (>>SHFT) and resolves the state index. The
// `& mask` is the masking the prior set_val omitted — out-of-mask bits are
// stripped, not leaked into the state lookup.
#[test]
fn test_mbbi_readback_masks_shifts_and_maps_state() {
    use epics_base_rs::server::records::mbbi::MbbiRecord;
    let mut rec = MbbiRecord::new(0);
    rec.put_field("ONVL", EpicsValue::ULong(1)).unwrap();
    rec.put_field("TWVL", EpicsValue::ULong(2)).unwrap();
    rec.init_record(0).unwrap(); // computes sdef=true
    rec.mask = 0x0C; // bits 2-3
    rec.shft = 2;
    // raw 0x08 -> masked 0x08 -> shifted (>>2) = 2 -> TWVL=2 -> state index 2.
    assert!(
        rec.apply_raw_readback(0x08),
        "mbbi claims the device readback"
    );
    assert_eq!(rec.rval, 0x08, "RVAL = raw & mask (C processMbbi)");
    assert_eq!(rec.val, 2, "shifted raw 2 -> state index 2 (TWVL)");
    // Mask gating: an out-of-mask 0x80 bit is stripped, NOT leaked. Before the
    // fix set_val left rval unmasked, so 0x88 would have shifted to 0x22 and
    // missed the state table (val 65535).
    assert!(rec.apply_raw_readback(0x88));
    assert_eq!(rec.rval, 0x08, "out-of-mask 0x80 bit masked away");
    assert_eq!(rec.val, 2, "still resolves to TWVL, not an unknown state");
}

// mbbi Soft Channel set_val is now a pass-through (C devMbbiSoft read_mbbi
// returns 2: the link value lands in VAL as the index, no state-table map).
// Before the fix set_val reverse-mapped a numeric link through raw_to_val,
// diverging from C whenever the source value was not a state RVAL.
#[test]
fn test_mbbi_set_val_is_soft_channel_passthrough() {
    use epics_base_rs::server::records::mbbi::MbbiRecord;
    let mut rec = MbbiRecord::new(0);
    rec.put_field("ONVL", EpicsValue::ULong(1)).unwrap();
    rec.put_field("TWVL", EpicsValue::ULong(2)).unwrap();
    rec.init_record(0).unwrap();
    // A numeric soft-link value is the index, verbatim.
    rec.set_val(EpicsValue::Long(2)).unwrap();
    assert_eq!(rec.val, 2, "soft-link value is the index, not state-mapped");
    // A value matching no state RVAL still passes through (old: raw_to_val ->
    // 65535 unknown-state; C devMbbiSoft -> the value itself).
    rec.set_val(EpicsValue::Long(7)).unwrap();
    assert_eq!(
        rec.val, 7,
        "pass-through, not the 65535 unknown-state sentinel"
    );
    // Enum and ZRST..FFST string puts still resolve to the index directly.
    rec.set_val(EpicsValue::Enum(1)).unwrap();
    assert_eq!(rec.val, 1);
}

#[test]
fn test_bi_record() {
    let mut rec = BiRecord::new(0);
    assert_eq!(rec.record_type(), "bi");
    rec.put_field("VAL", EpicsValue::Enum(1)).unwrap();
    match rec.get_field("VAL") {
        Some(EpicsValue::Enum(v)) => assert_eq!(v, 1),
        other => panic!("expected Enum(1), got {:?}", other),
    }
    rec.put_field("ZNAM", EpicsValue::String("Off".into()))
        .unwrap();
    rec.put_field("ONAM", EpicsValue::String("On".into()))
        .unwrap();
    match rec.get_field("ZNAM") {
        Some(EpicsValue::String(s)) => assert_eq!(s, "Off"),
        other => panic!("expected String, got {:?}", other),
    }
}

// epics-base f2fe9d12 (devBiSoftRaw): "Raw Soft Channel" INP reads
// must apply MASK to RVAL before the RVAL→VAL conversion. Verifies the
// `Record::raw_soft_input` override on BiRecord.
#[test]
fn test_bi_raw_soft_channel_applies_mask() {
    let mut rec = BiRecord::new(0);
    rec.mask = 0x0F;
    rec.raw_soft_input(RawSoftEntry::Read, EpicsValue::Long(0xFF))
        .expect("bi has a SoftRaw dset")
        .unwrap();
    assert_eq!(rec.rval, 0x0F, "mask must clamp RVAL to low nibble");
    let _ = rec.process().unwrap();
    match rec.get_field("VAL") {
        Some(EpicsValue::Enum(v)) => assert_eq!(v, 1, "masked-non-zero RVAL → VAL=1"),
        other => panic!("expected Enum, got {:?}", other),
    }
}

// MASK=0 must leave RVAL untouched (idempotent passthrough).
#[test]
fn test_bi_raw_soft_channel_mask_zero_passthrough() {
    let mut rec = BiRecord::new(0);
    rec.mask = 0;
    rec.raw_soft_input(RawSoftEntry::Read, EpicsValue::Long(0xDEAD_BEEFu32 as i32))
        .expect("bi has a SoftRaw dset")
        .unwrap();
    // RVAL is DBF_ULONG (biRecord.dbd.pod:199); same bit pattern, unsigned.
    assert_eq!(rec.rval, 0xDEAD_BEEF_u32);
}

// DBF_ULONG high-bit round-trip: a MASK/RVAL value >= 2^31 must survive
// without sign loss — the regression an i32 storage would introduce. C
// declares bo.MASK/RVAL as DBF_ULONG (boRecord.dbd.pod:261/252).
#[test]
fn test_bo_mask_rval_high_bit_round_trip() {
    use epics_base_rs::server::records::bo::BoRecord;
    let mut rec = BoRecord::new(0);
    rec.put_field("MASK", EpicsValue::ULong(0x8000_0000))
        .unwrap();
    assert_eq!(rec.get_field("MASK"), Some(EpicsValue::ULong(0x8000_0000)));
    rec.put_field("RVAL", EpicsValue::ULong(0xDEAD_BEEF))
        .unwrap();
    assert_eq!(rec.get_field("RVAL"), Some(EpicsValue::ULong(0xDEAD_BEEF)));
}

// DBF_ULONG high-bit round-trip for the mbbo state-value table: a ZRVL
// >= 2^31 must survive read-back and flow into RVAL through convert()
// without sign loss. C declares mbbo.ZRVL..FFVL and RVAL as DBF_ULONG
// (mbboRecord.dbd.pod:222/620).
#[test]
fn test_mbbo_zrvl_high_bit_round_trip() {
    use epics_base_rs::server::records::mbbo::MbboRecord;
    let mut rec = MbboRecord::new(0);
    rec.put_field("ZRVL", EpicsValue::ULong(0x8000_0000))
        .unwrap();
    assert_eq!(rec.get_field("ZRVL"), Some(EpicsValue::ULong(0x8000_0000)));
    // With a defined state table and VAL=0, the init tail's convert() copies
    // ZRVL into RVAL (C `mbboRecord.c:177`, after the constant-DOL load); the
    // high bit must not be lost to sign.
    rec.init_record(0).unwrap();
    rec.init_record_tail();
    assert_eq!(rec.get_field("RVAL"), Some(EpicsValue::ULong(0x8000_0000)));
}

// A masked-to-zero raw read must yield VAL=0 even when the source
// had bits outside the mask set.
#[test]
fn test_bi_raw_soft_channel_mask_to_zero() {
    let mut rec = BiRecord::new(1);
    rec.mask = 0x01;
    rec.raw_soft_input(RawSoftEntry::Read, EpicsValue::Long(0xFE))
        .expect("bi has a SoftRaw dset")
        .unwrap();
    assert_eq!(rec.rval, 0);
    let _ = rec.process().unwrap();
    match rec.get_field("VAL") {
        Some(EpicsValue::Enum(v)) => assert_eq!(v, 0),
        other => panic!("expected Enum, got {:?}", other),
    }
}

#[test]
fn test_stringin_record() {
    let rec = StringinRecord::new("hello");
    assert_eq!(rec.record_type(), "stringin");
    match rec.get_field("VAL") {
        Some(EpicsValue::String(s)) => assert_eq!(s, "hello"),
        other => panic!("expected String, got {:?}", other),
    }
}

#[test]
fn test_val_and_set_val() {
    let mut rec = AiRecord::new(5.0);
    match rec.val() {
        Some(EpicsValue::Double(v)) => assert!((v - 5.0).abs() < 1e-10),
        other => panic!("expected Double(5.0), got {:?}", other),
    }
    rec.set_val(EpicsValue::Double(10.0)).unwrap();
    match rec.val() {
        Some(EpicsValue::Double(v)) => assert!((v - 10.0).abs() < 1e-10),
        other => panic!("expected Double(10.0), got {:?}", other),
    }
}

#[test]
fn test_record_instance() {
    let rec = AiRecord::new(25.0);
    let instance = RecordInstance::new("TEMP".into(), rec);
    assert_eq!(instance.name, "TEMP");
    match instance.record.get_field("VAL") {
        Some(EpicsValue::Double(v)) => assert!((v - 25.0).abs() < 1e-10),
        other => panic!("expected Double(25.0), got {:?}", other),
    }
}

#[test]
fn test_read_only_field() {
    use epics_macros_rs::EpicsRecord;

    #[derive(EpicsRecord)]
    #[record(type = "test", crate_path = "epics_base_rs")]
    struct TestRecord {
        #[field(type = "Double")]
        pub val: f64,
        #[field(type = "String", read_only)]
        pub name: String,
    }

    let mut rec = TestRecord {
        val: 1.0,
        name: "fixed".into(),
    };

    match rec.get_field("NAME") {
        Some(EpicsValue::String(s)) => assert_eq!(s, "fixed"),
        other => panic!("expected String, got {:?}", other),
    }

    let result = rec.put_field("NAME", EpicsValue::String("changed".into()));
    assert!(result.is_err());

    rec.put_field("VAL", EpicsValue::Double(2.0)).unwrap();
    match rec.get_field("VAL") {
        Some(EpicsValue::Double(v)) => assert!((v - 2.0).abs() < 1e-10),
        other => panic!("expected Double(2.0), got {:?}", other),
    }

    // No `field_list()` assertion here: `#[derive(EpicsRecord)]` is not a
    // declaration source. `#[field(read_only)]` drives this record's own
    // `put_field` refusal (asserted above); the wire-visible SPC_NOMOD
    // declaration comes from the `.dbd`, and record type "test" has none.
}

#[test]
fn test_parse_pv_name() {
    use epics_base_rs::server::database::parse_pv_name;
    assert_eq!(parse_pv_name("TEMP"), ("TEMP", "VAL"));
    assert_eq!(parse_pv_name("TEMP.EGU"), ("TEMP", "EGU"));
    assert_eq!(parse_pv_name("TEMP.HOPR"), ("TEMP", "HOPR"));
    assert_eq!(parse_pv_name("A.B.C"), ("A.B", "C"));
}

#[test]
fn test_resolve_field_priority() {
    let rec = AiRecord::new(25.0);
    let instance = RecordInstance::new("TEMP".into(), rec);

    assert!(matches!(
        instance.resolve_field("VAL"),
        Some(EpicsValue::Double(_))
    ));
    assert!(matches!(
        instance.resolve_field("SEVR"),
        Some(EpicsValue::Short(0))
    ));
    assert!(matches!(
        instance.resolve_field("SCAN"),
        Some(EpicsValue::Enum(0))
    ));
    match instance.resolve_field("NAME") {
        Some(EpicsValue::String(s)) => assert_eq!(s, "TEMP"),
        other => panic!("expected String(TEMP), got {:?}", other),
    }
    match instance.resolve_field("RTYP") {
        Some(EpicsValue::String(s)) => assert_eq!(s, "ai"),
        other => panic!("expected String(ai), got {:?}", other),
    }
    assert!(instance.resolve_field("HIHI").is_some());
    assert!(instance.resolve_field("NONEXISTENT").is_none());
}

#[test]
fn test_common_field_put() {
    let rec = AiRecord::new(25.0);
    let mut instance = RecordInstance::new("TEMP".into(), rec);

    let result = instance
        .put_common_field("SCAN", EpicsValue::String("1 second".into()))
        .unwrap();
    assert!(matches!(result, CommonFieldPutResult::ScanChanged { .. }));
    assert_eq!(instance.common.scan, ScanType::SEC1);

    instance
        .put_common_field("HIHI", EpicsValue::Double(100.0))
        .unwrap();
    assert_eq!(
        instance.common.analog_alarm.as_ref().unwrap().hihi,
        AlarmLimit::Double(100.0)
    );
}

#[test]
fn test_evaluate_alarms() {
    use epics_base_rs::server::recgbl;
    let rec = AiRecord::new(0.0);
    let mut instance = RecordInstance::new("TEMP".into(), rec);
    instance.common.udf = 0;

    instance
        .put_common_field("HIHI", EpicsValue::Double(100.0))
        .unwrap();
    instance
        .put_common_field("HHSV", EpicsValue::Short(AlarmSeverity::Major as i16))
        .unwrap();
    instance
        .put_common_field("HIGH", EpicsValue::Double(80.0))
        .unwrap();
    instance
        .put_common_field("HSV", EpicsValue::Short(AlarmSeverity::Minor as i16))
        .unwrap();

    instance.evaluate_alarms();
    recgbl::rec_gbl_reset_alarms(&mut instance.common);
    assert_eq!(instance.common.sevr, AlarmSeverity::NoAlarm);

    instance.record.set_val(EpicsValue::Double(85.0)).unwrap();
    instance.evaluate_alarms();
    recgbl::rec_gbl_reset_alarms(&mut instance.common);
    assert_eq!(instance.common.sevr, AlarmSeverity::Minor);

    instance.record.set_val(EpicsValue::Double(105.0)).unwrap();
    instance.evaluate_alarms();
    recgbl::rec_gbl_reset_alarms(&mut instance.common);
    assert_eq!(instance.common.sevr, AlarmSeverity::Major);
}

#[test]
fn test_parse_link_v2() {
    assert_eq!(parse_link_v2(""), ParsedLink::None);
    assert_eq!(parse_link_v2("  "), ParsedLink::None);

    assert_eq!(parse_link_v2("42"), ParsedLink::Constant("42".to_string()));
    assert_eq!(
        parse_link_v2("3.14"),
        ParsedLink::Constant("3.14".to_string())
    );
    assert_eq!(
        parse_link_v2("-1.5"),
        ParsedLink::Constant("-1.5".to_string())
    );

    // A modifier-less DB link defaults to NPP (`NoProcess`), matching
    // C `dbParseLink` (memset→0; `pvlOptPP` only on explicit ` PP`).
    // Was wrongly `ProcessPassive`.
    assert_eq!(
        parse_link_v2("TEMP"),
        ParsedLink::Db(DbLink::new(
            "TEMP",
            LinkProcessPolicy::NoProcess,
            MonitorSwitch::NoMaximize,
        ))
    );

    assert_eq!(
        parse_link_v2("TEMP.EGU"),
        ParsedLink::Db(DbLink::new(
            "TEMP.EGU",
            LinkProcessPolicy::NoProcess,
            MonitorSwitch::NoMaximize,
        ))
    );

    assert_eq!(
        parse_link_v2("TEMP.EGU NPP"),
        ParsedLink::Db(DbLink::new(
            "TEMP.EGU",
            LinkProcessPolicy::NoProcess,
            MonitorSwitch::NoMaximize,
        ))
    );

    // The two cases the deleted `parse_link`/`LinkAddress` wrapper covered
    // and nothing else did: an explicit `.VAL` carrying each process modifier.
    assert_eq!(
        parse_link_v2("TEMP.VAL PP"),
        ParsedLink::Db(DbLink::new(
            "TEMP.VAL",
            LinkProcessPolicy::ProcessPassive,
            MonitorSwitch::NoMaximize,
        ))
    );
    assert_eq!(
        parse_link_v2("TEMP.VAL NPP"),
        ParsedLink::Db(DbLink::new(
            "TEMP.VAL",
            LinkProcessPolicy::NoProcess,
            MonitorSwitch::NoMaximize,
        ))
    );

    assert_eq!(
        parse_link_v2("ca://PV:NAME"),
        ParsedLink::Ca(CaLink::new("PV:NAME"))
    );
    assert_eq!(
        parse_link_v2("pva://PV:NAME"),
        ParsedLink::Pva("PV:NAME".to_string())
    );

    // A quoted string is not a number, so C's `dbParseLink` falls through to
    // the PV-link arm: softIoc reports `INP: CA_LINK "hello" NPP NMS` for
    // `field(INP,"\"hello\"")`, with the quotes part of the channel name.
    assert!(matches!(parse_link_v2("\"hello\""), ParsedLink::Db(_)));

    let c = parse_link_v2("3.15");
    assert_eq!(c.constant_value(), Some(EpicsValue::Double(3.15)));
    assert_eq!(parse_link_v2("\"hello\"").constant_value(), None);
    assert_eq!(parse_link_v2("TEMP").constant_value(), None);
}

#[test]
fn test_link_cache_invalidation() {
    let rec = AiRecord::new(0.0);
    let mut instance = RecordInstance::new("TEMP".into(), rec);

    assert_eq!(instance.parsed_inp, ParsedLink::None);
    instance
        .put_common_field("INP", EpicsValue::String("SOURCE.VAL".into()))
        .unwrap();
    if let ParsedLink::Db(ref db) = instance.parsed_inp {
        assert_eq!(db.target().record, "SOURCE");
    } else {
        panic!("expected Db link");
    }

    instance
        .put_common_field("INP", EpicsValue::String("OTHER".into()))
        .unwrap();
    if let ParsedLink::Db(ref db) = instance.parsed_inp {
        assert_eq!(db.pvname(), "OTHER");
    } else {
        panic!("expected Db link");
    }

    instance
        .put_common_field("INP", EpicsValue::String("".into()))
        .unwrap();
    assert_eq!(instance.parsed_inp, ParsedLink::None);
}

#[test]
fn test_ai_linear_conversion() {
    let mut rec = AiRecord::default();
    rec.linr = 1;
    rec.eguf = 100.0;
    rec.egul = 0.0;
    rec.eslo = 1.0;
    rec.roff = 0;
    rec.aslo = 1.0;
    rec.aoff = 0.0;

    rec.rval = 50;
    rec.process().unwrap();
    assert!((rec.val - 50.0).abs() < 1e-10);
}

#[test]
fn test_ai_linear_with_offsets() {
    let mut rec = AiRecord::default();
    rec.linr = 2;
    rec.eoff = 10.0;
    rec.eslo = 0.5;
    rec.roff = 100;
    rec.aslo = 2.0;
    rec.aoff = 5.0;

    rec.rval = 200;
    rec.process().unwrap();
    assert!((rec.val - 312.5).abs() < 1e-10);
}

#[test]
fn test_ai_smoothing() {
    let mut rec = AiRecord::default();
    rec.linr = 1;
    rec.eslo = 1.0;
    rec.aslo = 1.0;
    rec.smoo = 0.5;
    // `init_record` is what arms the INIT phase (C `aiRecord.c:114`), and the
    // phase is what makes the first conversion SMOO's initial condition rather
    // than a blend against the pre-init VAL. A `.db` load always runs it.
    rec.init_record(0).unwrap();

    rec.rval = 100;
    rec.process().unwrap();
    assert!((rec.val - 100.0).abs() < 1e-10);
    assert!(
        !rec.init.is_initial(),
        "C clears INIT at the end of every process (aiRecord.c:170)"
    );

    rec.rval = 200;
    rec.process().unwrap();
    assert!((rec.val - 150.0).abs() < 1e-10);
}

#[test]
fn test_ai_no_conversion() {
    let mut rec = AiRecord::default();
    rec.linr = 0;
    rec.rval = 42;
    rec.process().unwrap();
    assert!((rec.val - 42.0).abs() < 1e-10);
}

#[test]
fn test_common_fields_desc() {
    let rec = AiRecord::new(25.0);
    let mut instance = RecordInstance::new("TEMP".into(), rec);

    instance
        .put_common_field("DESC", EpicsValue::String("Temperature".into()))
        .unwrap();
    match instance.get_common_field("DESC") {
        Some(EpicsValue::String(s)) => assert_eq!(s, "Temperature"),
        other => panic!("expected String, got {:?}", other),
    }
    match instance.resolve_field("DESC") {
        Some(EpicsValue::String(s)) => assert_eq!(s, "Temperature"),
        other => panic!("expected String, got {:?}", other),
    }
}

#[test]
fn test_common_fields_new() {
    let rec = AiRecord::new(0.0);
    let mut instance = RecordInstance::new("TEST".into(), rec);

    assert_eq!(instance.common.phas, 0);
    instance
        .put_common_field("PHAS", EpicsValue::Short(2))
        .unwrap();
    assert_eq!(instance.common.phas, 2);

    assert_eq!(instance.common.disv, 1);

    instance
        .put_common_field("HYST", EpicsValue::Double(5.0))
        .unwrap();
    assert!((instance.common.hyst - 5.0).abs() < 1e-10);
}

#[test]
fn test_hyst_alarm_hysteresis() {
    use epics_base_rs::server::recgbl;
    let rec = AiRecord::new(0.0);
    let mut instance = RecordInstance::new("TEMP".into(), rec);
    instance.common.udf = 0;

    instance
        .put_common_field("HIGH", EpicsValue::Double(80.0))
        .unwrap();
    instance
        .put_common_field("HSV", EpicsValue::Short(AlarmSeverity::Minor as i16))
        .unwrap();
    instance
        .put_common_field("HYST", EpicsValue::Double(5.0))
        .unwrap();

    instance.record.set_val(EpicsValue::Double(85.0)).unwrap();
    instance.evaluate_alarms();
    recgbl::rec_gbl_reset_alarms(&mut instance.common);
    assert_eq!(instance.common.sevr, AlarmSeverity::Minor);

    instance.record.set_val(EpicsValue::Double(82.0)).unwrap();
    instance.evaluate_alarms();
    recgbl::rec_gbl_reset_alarms(&mut instance.common);
    assert_eq!(instance.common.sevr, AlarmSeverity::Minor);

    instance.record.set_val(EpicsValue::Double(78.0)).unwrap();
    instance.evaluate_alarms();
    recgbl::rec_gbl_reset_alarms(&mut instance.common);
    // C: lalm=80, val=78 >= 80-5=75, so alarm stays Minor
    assert_eq!(instance.common.sevr, AlarmSeverity::Minor);

    instance.record.set_val(EpicsValue::Double(76.0)).unwrap();
    instance.evaluate_alarms();
    recgbl::rec_gbl_reset_alarms(&mut instance.common);
    // C: lalm=80, val=76 >= 80-5=75, alarm still Minor (within hysteresis)
    assert_eq!(instance.common.sevr, AlarmSeverity::Minor);

    // Below hysteresis: val=74 < 75, alarm clears
    instance.record.set_val(EpicsValue::Double(74.0)).unwrap();
    instance.evaluate_alarms();
    recgbl::rec_gbl_reset_alarms(&mut instance.common);
    assert_eq!(instance.common.sevr, AlarmSeverity::NoAlarm);
}

#[test]
fn test_deadband_mdel() {
    // MDEL gates the DBE_VALUE class only. ADEL=0 archives every
    // actual change (C `recGblCheckDeadband` with deadband 0 fires on
    // any non-zero delta, aiRecord.c `monitor()` posts DBE_ARCHIVE),
    // so a sub-MDEL change still posts VAL — with DBE_LOG alone.
    use epics_base_rs::server::recgbl::EventMask;
    let mut rec = AiRecord::default();
    rec.mdel = 5.0;
    rec.adel = 0.0;
    let mut instance = RecordInstance::new("TEST".into(), rec);
    let val_mask = |snap: &epics_base_rs::server::record::ProcessSnapshot| {
        snap.changed_fields
            .iter()
            .find(|(k, _, _)| k == "VAL")
            .map(|(_, _, m)| *m)
            .unwrap_or(EventMask::NONE)
    };

    // Unchanged value on the FIRST process: neither deadband fires, but the
    // record leaves its born-UDF alarm (STAT=UDF, dbCommon.dbd `initial("UDF")`)
    // for NO_ALARM, so `recGblResetAlarms` returns DBE_ALARM and C posts VAL
    // with a non-zero monitor_mask anyway (`aiRecord.c::monitor`: `if
    // (monitor_mask) db_post_events(prec, &prec->val, monitor_mask)`).
    instance.record.set_val(EpicsValue::Double(0.0)).unwrap();
    instance.record.set_device_did_compute(true);
    let (snap, _alarm_posts) = instance.process_local().unwrap();
    assert_eq!(val_mask(&snap), EventMask::ALARM);

    // |3-0| < MDEL=5: VALUE throttled; ADEL=0 archives the change.
    instance.record.set_val(EpicsValue::Double(3.0)).unwrap();
    instance.record.set_device_did_compute(true);
    let (snap, _alarm_posts) = instance.process_local().unwrap();
    assert_eq!(val_mask(&snap), EventMask::LOG);

    // |6-0| > 5: both classes fire, MLST -> 6.
    instance.record.set_val(EpicsValue::Double(6.0)).unwrap();
    instance.record.set_device_did_compute(true);
    let (snap, _alarm_posts) = instance.process_local().unwrap();
    assert_eq!(val_mask(&snap), EventMask::VALUE | EventMask::LOG);

    // |10-6| < 5: VALUE throttled again.
    instance.record.set_val(EpicsValue::Double(10.0)).unwrap();
    instance.record.set_device_did_compute(true);
    let (snap, _alarm_posts) = instance.process_local().unwrap();
    assert_eq!(val_mask(&snap), EventMask::LOG);

    // |12-6| > 5: both classes fire.
    instance.record.set_val(EpicsValue::Double(12.0)).unwrap();
    instance.record.set_device_did_compute(true);
    let (snap, _alarm_posts) = instance.process_local().unwrap();
    assert_eq!(val_mask(&snap), EventMask::VALUE | EventMask::LOG);
}

#[test]
fn test_deadband_mdel_zero() {
    use epics_base_rs::server::recgbl::EventMask;
    let mut rec = AiRecord::default();
    rec.mdel = 0.0;
    let mut instance = RecordInstance::new("TEST".into(), rec);

    // MDEL=0 does not fire on a zero delta; the VAL post this first process
    // does carry is the born-UDF -> NO_ALARM transition (DBE_ALARM), which C
    // folds into the same `db_post_events(&prec->val, monitor_mask)`.
    instance.record.set_val(EpicsValue::Double(0.0)).unwrap();
    instance.record.set_device_did_compute(true);
    let (snap, _alarm_posts) = instance.process_local().unwrap();
    assert_eq!(
        snap.changed_fields
            .iter()
            .find(|(k, _, _)| k == "VAL")
            .map(|(_, _, m)| *m),
        Some(EventMask::ALARM),
        "no deadband fired — only the initial alarm transition"
    );

    instance.record.set_val(EpicsValue::Double(0.001)).unwrap();
    instance.record.set_device_did_compute(true);
    let (snap, _alarm_posts) = instance.process_local().unwrap();
    assert!(snap.changed_fields.iter().any(|(k, _, _)| k == "VAL"));
}

#[test]
fn test_deadband_mdel_negative() {
    let mut rec = AiRecord::default();
    rec.mdel = -1.0;
    let mut instance = RecordInstance::new("TEST".into(), rec);

    instance.record.set_val(EpicsValue::Double(0.0)).unwrap();
    instance.record.set_device_did_compute(true);
    let (snap, _alarm_posts) = instance.process_local().unwrap();
    assert!(snap.changed_fields.iter().any(|(k, _, _)| k == "VAL"));
}

#[test]
fn test_bi_state_alarm() {
    use epics_base_rs::server::recgbl;
    let mut rec = BiRecord::new(0);
    rec.zsv = AlarmSeverity::Major as i16;
    rec.osv = AlarmSeverity::Minor as i16;

    let mut instance = RecordInstance::new("SW".into(), rec);
    instance.common.udf = 0;

    // bi STATE alarm lives in the `Record::check_alarms` hook (C
    // `biRecord.c::checkAlarms`); `process_local` calls it before
    // `evaluate_alarms`. Mirror that order here.
    instance.record.check_alarms(&mut instance.common);
    instance.evaluate_alarms();
    recgbl::rec_gbl_reset_alarms(&mut instance.common);
    assert_eq!(instance.common.sevr, AlarmSeverity::Major);

    instance.record.set_val(EpicsValue::Enum(1)).unwrap();
    instance.record.check_alarms(&mut instance.common);
    instance.evaluate_alarms();
    recgbl::rec_gbl_reset_alarms(&mut instance.common);
    assert_eq!(instance.common.sevr, AlarmSeverity::Minor);
}

#[test]
fn test_mbbi_state_alarm() {
    use epics_base_rs::server::recgbl;
    use epics_base_rs::server::records::mbbi::MbbiRecord;

    let mut rec = MbbiRecord::new(0);
    rec.onsv = AlarmSeverity::Minor as i16;
    rec.twsv = AlarmSeverity::Major as i16;

    let mut instance = RecordInstance::new("SEL".into(), rec);
    instance.common.udf = 0;

    // mbbi STATE alarm lives in the `Record::check_alarms` hook (C
    // `mbbiRecord.c::checkAlarms`); `process_local` calls it before
    // `evaluate_alarms`. Mirror that order here.
    instance.record.check_alarms(&mut instance.common);
    instance.evaluate_alarms();
    recgbl::rec_gbl_reset_alarms(&mut instance.common);
    assert_eq!(instance.common.sevr, AlarmSeverity::NoAlarm);

    instance.record.set_val(EpicsValue::Enum(1)).unwrap();
    instance.record.check_alarms(&mut instance.common);
    instance.evaluate_alarms();
    recgbl::rec_gbl_reset_alarms(&mut instance.common);
    assert_eq!(instance.common.sevr, AlarmSeverity::Minor);

    instance.record.set_val(EpicsValue::Enum(2)).unwrap();
    instance.record.check_alarms(&mut instance.common);
    instance.evaluate_alarms();
    recgbl::rec_gbl_reset_alarms(&mut instance.common);
    assert_eq!(instance.common.sevr, AlarmSeverity::Major);
}

#[test]
fn test_mbbi_unsv() {
    use epics_base_rs::server::records::mbbi::MbbiRecord;

    let mut rec = MbbiRecord::new(0);
    rec.unsv = AlarmSeverity::Invalid as i16;

    let mut instance = RecordInstance::new("SEL".into(), rec);

    instance.record.set_val(EpicsValue::Enum(15)).unwrap();
    instance.evaluate_alarms();
    assert_eq!(instance.common.sevr, AlarmSeverity::NoAlarm);
}

#[test]
fn test_deadband_alarm_on_change_bypasses_value_deadband() {
    // C `recGbl.c:202-222` (`recGblResetAlarms`): SEVR is posted only
    // when `prev_sevr != new_sevr`, and STAT only when `stat_mask` is
    // set (sevr change / stat change / amsg change). The alarm-field
    // posts are NOT gated by the VAL monitor deadband (MDEL/ADEL) —
    // `db_post_events(&pdbc->stat, …)` runs independently of the
    // value-change check. This test verifies the C-correct behavior:
    // a genuine SEVR transition posts SEVR and STAT even though the
    // VAL change is smaller than MDEL. `process_local` returns
    // SEVR/STAT in `alarm_posts` (each with its own C event mask),
    // not in the `changed_fields` snapshot. VAL itself still posts —
    // with `val_mask = DBE_ALARM` alone (recGbl.c:212; ai `monitor()`
    // posts VAL whenever `monitor_mask` is non-zero), so a
    // `DBE_ALARM`-only subscriber sees the value at the alarm moment
    // while `DBE_VALUE` subscribers stay deadband-throttled.
    use epics_base_rs::server::recgbl::EventMask;
    let mut rec = AiRecord::default();
    rec.mdel = 100.0; // VAL change of 1.0 is below the value deadband.
    let mut instance = RecordInstance::new("TEST".into(), rec);
    // HIGH=0.5/Major so VAL=1.0 trips a HIGH alarm — a real
    // NoAlarm -> Major SEVR transition.
    instance.common.analog_alarm = Some(AnalogAlarmConfig {
        hihi: AlarmLimit::Double(1000.0),
        high: AlarmLimit::Double(0.5),
        low: AlarmLimit::Double(-1000.0),
        lolo: AlarmLimit::Double(-2000.0),
        hhsv: AlarmSeverity::Major as i16,
        hsv: AlarmSeverity::Major as i16,
        lsv: AlarmSeverity::Minor as i16,
        llsv: AlarmSeverity::Major as i16,
    });

    instance.record.set_val(EpicsValue::Double(1.0)).unwrap();
    instance.record.set_device_did_compute(true);
    let (snap, alarm_posts) = instance.process_local().unwrap();
    // VAL's DBE_VALUE class is deadband-filtered (|1.0 - 0.0| <
    // MDEL=100), but the default ADEL=0 archives the change (DBE_LOG)
    // and the alarm transition adds `val_mask = DBE_ALARM`
    // (recGbl.c:212) — so VAL posts with DBE_LOG|DBE_ALARM, never
    // DBE_VALUE.
    let val_mask = snap
        .changed_fields
        .iter()
        .find(|(k, _, _)| k == "VAL")
        .map(|(_, _, m)| *m);
    assert_eq!(
        val_mask,
        Some(EventMask::LOG | EventMask::ALARM),
        "alarm transition under a silent MDEL must post VAL with \
         DBE_LOG|DBE_ALARM, without DBE_VALUE"
    );
    // SEVR / STAT are NOT in the snapshot — they ride the per-field
    // `alarm_posts` list instead.
    assert!(!snap.changed_fields.iter().any(|(k, _, _)| k == "SEVR"));
    assert!(!snap.changed_fields.iter().any(|(k, _, _)| k == "STAT"));
    // SEVR posted DBE_VALUE on a sevr change.
    let sevr_mask = alarm_posts
        .iter()
        .find(|(f, _)| *f == "SEVR")
        .map(|(_, m)| *m);
    assert_eq!(
        sevr_mask,
        Some(EventMask::VALUE),
        "SEVR must post with DBE_VALUE only"
    );
    // STAT posted DBE_ALARM (sevr change) | DBE_VALUE (stat change).
    let stat_mask = alarm_posts
        .iter()
        .find(|(f, _)| *f == "STAT")
        .map(|(_, m)| *m);
    assert_eq!(
        stat_mask,
        Some(EventMask::ALARM | EventMask::VALUE),
        "STAT must post with DBE_ALARM | DBE_VALUE on a sevr+stat change"
    );
    // Defect 2: AMSG must be posted alongside STAT with the SAME mask.
    // C `recGblResetAlarms` posts AMSG whenever any alarm field moved;
    // `process_local` previously omitted it entirely.
    let amsg_mask = alarm_posts
        .iter()
        .find(|(f, _)| *f == "AMSG")
        .map(|(_, m)| *m);
    assert_eq!(
        amsg_mask, stat_mask,
        "AMSG must be posted with the same mask as STAT"
    );
}

#[test]
fn test_per_field_masks_narrow_deadband_and_aux_posts() {
    // C posts each field with its own mask in one `db_post_events`
    // call per field; one record-wide mask collapses that. Two rules
    // pinned here (aiRecord.c `monitor()` 460-465):
    //   * the deadband-tracked field narrows to the deadbands that
    //     crossed — ADEL-only crossing posts DBE_LOG, NOT DBE_VALUE,
    //     even when another field changed in the same pass (the
    //     pre-fix record-wide mask leaked that field's DBE_VALUE into
    //     the deadband post, breaking the MDEL throttle for
    //     DBE_VALUE-only subscribers);
    //   * ai posts RVAL with VAL's OWN monitor_mask, nested in
    //     `if (monitor_mask)` (aiRecord.c:463) — NOT a forced
    //     DBE_VALUE|DBE_LOG. With MDEL uncrossed and only ADEL crossed,
    //     monitor_mask is DBE_LOG, so RVAL posts DBE_LOG alone (a
    //     DBE_VALUE-only subscriber must not see it). The aux fields C
    //     posts with `monitor_mask | DBE_VALUE | DBE_LOG` (calc A..L,
    //     ao RVAL) are a different family, exercised elsewhere.
    use epics_base_rs::server::recgbl::EventMask;
    use epics_base_rs::types::DbFieldType;
    let mut rec = AiRecord::default();
    rec.mdel = 100.0; // VALUE class stays deadband-throttled
    rec.adel = 0.5; // LOG class fires on |delta| > 0.5
    let mut instance = RecordInstance::new("TEST".into(), rec);
    let _rval_rx = instance
        .add_subscriber("RVAL", 1, DbFieldType::Long, EventMask::VALUE.bits())
        .expect("RVAL subscriber");

    // Priming pass: MLST/ALST start at the never-posted sentinel, so
    // the first cycle posts VAL unconditionally and seeds both.
    instance.record.set_device_did_compute(true);
    let _ = instance.process_local().unwrap();

    // ADEL crosses (|1.0 - 0.0| > 0.5), MDEL does not (< 100), and
    // RVAL changes in the same pass.
    instance.record.set_val(EpicsValue::Double(1.0)).unwrap();
    instance
        .record
        .put_field("RVAL", EpicsValue::Long(42))
        .unwrap();
    instance.record.set_device_did_compute(true);
    let (snap, _) = instance.process_local().unwrap();
    let mask_of = |f: &str| {
        snap.changed_fields
            .iter()
            .find(|(k, _, _)| k == f)
            .map(|(_, _, m)| *m)
    };
    assert_eq!(
        mask_of("VAL"),
        Some(EventMask::LOG),
        "ADEL-only crossing posts VAL with DBE_LOG alone; the changed \
         RVAL must not leak DBE_VALUE into the deadband post"
    );
    assert_eq!(
        mask_of("RVAL"),
        Some(EventMask::LOG),
        "ai RVAL posts with VAL's raw monitor_mask (DBE_LOG here, MDEL \
         uncrossed), not a forced DBE_VALUE|DBE_LOG (aiRecord.c:463)"
    );
}

#[test]
fn test_acks_posts_once_with_dbe_value_only() {
    // W10-E1. C `recGblResetAlarms` posts ACKS EXACTLY ONCE, with a literal
    // `DBE_VALUE`, from inside `if (stat_mask)` (recGbl.c:214-217):
    //
    //     if (!pdbc->ackt || new_sevr >= pdbc->acks) {
    //         pdbc->acks = new_sevr;
    //         db_post_events(pdbc, &pdbc->acks, DBE_VALUE);
    //     }
    //
    // There is no second ACKS post anywhere in C. The port's generic
    // change-detection loop (`collect_subscriber_posts`) did not exclude ACKS,
    // so a changed ACKS was ALSO emitted in the record-wide snapshot carrying
    // `alarm_bits | DBE_VALUE | DBE_LOG` — a `.ACKS` monitor saw two events, one
    // of them with a mask C never uses for that field.
    use epics_base_rs::server::recgbl::EventMask;
    use epics_base_rs::types::DbFieldType;
    let mut rec = AiRecord::default();
    rec.mdel = 100.0;
    let mut instance = RecordInstance::new("TEST".into(), rec);
    let _acks_rx = instance
        .add_subscriber("ACKS", 1, DbFieldType::Enum, EventMask::VALUE.bits())
        .expect("ACKS subscriber");
    // HIGH=0.5/Major so VAL=1.0 raises a NoAlarm -> Major transition, which
    // makes `recGblResetAlarms` fire the ack rule and move ACKS 0 -> 2.
    instance.common.analog_alarm = Some(AnalogAlarmConfig {
        hihi: AlarmLimit::Double(1000.0),
        high: AlarmLimit::Double(0.5),
        low: AlarmLimit::Double(-1000.0),
        lolo: AlarmLimit::Double(-2000.0),
        hhsv: AlarmSeverity::Major as i16,
        hsv: AlarmSeverity::Major as i16,
        lsv: AlarmSeverity::Minor as i16,
        llsv: AlarmSeverity::Major as i16,
    });

    instance.record.set_val(EpicsValue::Double(1.0)).unwrap();
    instance.record.set_device_did_compute(true);
    let (snap, alarm_posts) = instance.process_local().unwrap();

    assert_eq!(
        instance.common.acks,
        AlarmSeverity::Major,
        "the ack rule fired and raised ACKS"
    );
    // The ONLY ACKS post is the alarm-field one, with DBE_VALUE.
    let acks_posts: Vec<_> = alarm_posts.iter().filter(|(f, _)| *f == "ACKS").collect();
    assert_eq!(acks_posts.len(), 1, "exactly one ACKS post");
    assert_eq!(
        *acks_posts[0],
        ("ACKS", EventMask::VALUE),
        "C posts ACKS with a literal DBE_VALUE (recGbl.c:216)"
    );
    // ...and NOT a second time from the record-wide change-detection snapshot.
    assert!(
        !snap.changed_fields.iter().any(|(k, _, _)| k == "ACKS"),
        "ACKS must not also ride the generic snapshot — C posts it once"
    );
}

#[test]
fn test_no_alarm_change_does_not_post_sevr_stat() {
    // C `recGbl.c:202-208`: when `prev_sevr == new_sevr` and
    // `prev_stat == new_stat`, `recGblResetAlarms` posts neither
    // SEVR nor STAT. A record processed with no alarm transition
    // must not emit alarm-field monitor events — neither in the
    // record-wide snapshot nor in the per-field `alarm_posts` list.
    let mut rec = AiRecord::default();
    rec.mdel = 100.0;
    let mut instance = RecordInstance::new("TEST".into(), rec);
    // The FIRST process is an alarm transition — a record is born
    // `STAT=UDF` (dbCommon.dbd `initial("UDF")`) and clears it here — so it
    // posts STAT/SEVR in C too. The no-transition cycle is the next one.
    instance.record.set_val(EpicsValue::Double(1.0)).unwrap();
    instance.record.set_device_did_compute(true);
    let (_first, first_posts) = instance.process_local().unwrap();
    assert!(
        first_posts.iter().any(|(f, _)| *f == "STAT"),
        "STAT moved UDF -> NO_ALARM, and C posts the field that changed \
         (recGbl.c:206-208). SEVR did not move on this bare instance, so C \
         posts no SEVR (recGbl.c:202-205)."
    );

    instance.record.set_val(EpicsValue::Double(1.0)).unwrap();
    instance.record.set_device_did_compute(true);
    let (snap, alarm_posts) = instance.process_local().unwrap();
    assert!(!snap.changed_fields.iter().any(|(k, _, _)| k == "SEVR"));
    assert!(!snap.changed_fields.iter().any(|(k, _, _)| k == "STAT"));
    assert!(!alarm_posts.iter().any(|(f, _)| *f == "SEVR"));
    assert!(!alarm_posts.iter().any(|(f, _)| *f == "STAT"));
}

#[test]
fn test_alarm_cycle_does_not_fan_out_for_default_records() {
    // The alarm-cycle fanout (posting unchanged monitored fields with
    // DBE_ALARM) is record-type-specific: C motorRecord.cc `monitor()`
    // posts its whole list once `monitor_mask != 0`, but
    // aiRecord.c `monitor()` posts only VAL with `monitor_mask` and
    // RVAL when it actually changed. `alarm_cycle_monitored_fields`
    // defaults to empty, so an ai alarm transition must NOT post an
    // unchanged subscribed RVAL.
    use epics_base_rs::server::recgbl::EventMask;
    use epics_base_rs::types::DbFieldType;
    let mut rec = AiRecord::default();
    rec.mdel = 100.0;
    let mut instance = RecordInstance::new("TEST".into(), rec);
    instance.common.analog_alarm = Some(AnalogAlarmConfig {
        hihi: AlarmLimit::Double(1000.0),
        high: AlarmLimit::Double(0.5),
        low: AlarmLimit::Double(-1000.0),
        lolo: AlarmLimit::Double(-2000.0),
        hhsv: AlarmSeverity::Major as i16,
        hsv: AlarmSeverity::Major as i16,
        lsv: AlarmSeverity::Minor as i16,
        llsv: AlarmSeverity::Major as i16,
    });
    let _rval_rx = instance
        .add_subscriber("RVAL", 1, DbFieldType::Long, EventMask::ALARM.bits())
        .expect("RVAL subscriber");

    // VAL=1.0 trips HIGH/Major — a real alarm transition; RVAL is
    // untouched.
    instance.record.set_val(EpicsValue::Double(1.0)).unwrap();
    instance.record.set_device_did_compute(true);
    let (snap, _alarm_posts) = instance.process_local().unwrap();
    assert!(
        snap.changed_fields.iter().any(|(k, _, _)| k == "VAL"),
        "the alarm transition posts VAL"
    );
    assert!(
        !snap.changed_fields.iter().any(|(k, _, _)| k == "RVAL"),
        "ai has no alarm-cycle fanout list: unchanged RVAL must not post"
    );
}

#[test]
fn test_ai_rval_not_posted_when_val_within_deadband() {
    // C aiRecord.c:460-465 nests the RVAL post inside `if (monitor_mask)`:
    // when VAL stays within both MDEL and ADEL and no alarm changes,
    // monitor_mask == 0 and C posts NOTHING — not even a changed RVAL. The
    // pre-fix port posted RVAL through the generic aux path on any RVAL
    // change, over-notifying a client monitoring ai.RVAL directly under a
    // non-default MDEL.
    use epics_base_rs::server::recgbl::EventMask;
    use epics_base_rs::types::DbFieldType;
    let mut rec = AiRecord::default();
    rec.mdel = 100.0;
    rec.adel = 100.0;
    let mut instance = RecordInstance::new("TEST".into(), rec);
    let _rval_rx = instance
        .add_subscriber("RVAL", 1, DbFieldType::Long, EventMask::VALUE.bits())
        .expect("RVAL subscriber");
    // Priming pass seeds MLST/ALST and last_posted[RVAL].
    instance.record.set_device_did_compute(true);
    let _ = instance.process_local().unwrap();

    // VAL 0 -> 1: below MDEL and ADEL (no crossing), no alarm; RVAL 0 -> 42.
    instance.record.set_val(EpicsValue::Double(1.0)).unwrap();
    instance
        .record
        .put_field("RVAL", EpicsValue::Long(42))
        .unwrap();
    instance.record.set_device_did_compute(true);
    let (snap, _) = instance.process_local().unwrap();
    assert!(
        !snap.changed_fields.iter().any(|(k, _, _)| k == "VAL"),
        "VAL within both deadbands: monitor_mask == 0, VAL not posted"
    );
    assert!(
        !snap.changed_fields.iter().any(|(k, _, _)| k == "RVAL"),
        "RVAL must not post when monitor_mask == 0 (C nests it in `if(monitor_mask)`)"
    );
}

#[test]
fn test_ai_rval_alarm_only_cycle_posts_alarm_mask_not_value_log() {
    // On an alarm transition with VAL inside MDEL/ADEL, C monitor_mask is
    // DBE_ALARM, and ai posts RVAL with that raw mask (aiRecord.c:463) —
    // DBE_ALARM alone, NOT a forced DBE_VALUE|DBE_LOG. A DBE_VALUE-only
    // subscriber must not receive the RVAL event.
    use epics_base_rs::server::recgbl::EventMask;
    use epics_base_rs::types::DbFieldType;
    let mut rec = AiRecord::default();
    rec.mdel = 100.0;
    rec.adel = 100.0;
    let mut instance = RecordInstance::new("TEST".into(), rec);
    instance.common.analog_alarm = Some(AnalogAlarmConfig {
        hihi: AlarmLimit::Double(1000.0),
        high: AlarmLimit::Double(0.5),
        low: AlarmLimit::Double(-1000.0),
        lolo: AlarmLimit::Double(-2000.0),
        hhsv: AlarmSeverity::Major as i16,
        hsv: AlarmSeverity::Major as i16,
        lsv: AlarmSeverity::Minor as i16,
        llsv: AlarmSeverity::Major as i16,
    });
    let _rval_rx = instance
        .add_subscriber("RVAL", 1, DbFieldType::Long, EventMask::ALARM.bits())
        .expect("RVAL subscriber");
    // Prime at VAL=0 (no alarm), seeding MLST/ALST and last_posted[RVAL].
    instance.record.set_device_did_compute(true);
    let _ = instance.process_local().unwrap();

    // VAL=1.0 trips HIGH/Major; VAL stays within MDEL/ADEL of 0; RVAL 0 -> 7.
    instance.record.set_val(EpicsValue::Double(1.0)).unwrap();
    instance
        .record
        .put_field("RVAL", EpicsValue::Long(7))
        .unwrap();
    instance.record.set_device_did_compute(true);
    let (snap, _) = instance.process_local().unwrap();
    let rval_mask = snap
        .changed_fields
        .iter()
        .find(|(k, _, _)| k == "RVAL")
        .map(|(_, _, m)| *m);
    assert_eq!(
        rval_mask,
        Some(EventMask::ALARM),
        "alarm-only cycle posts RVAL with DBE_ALARM alone (raw monitor_mask), not VALUE|LOG"
    );
}

#[test]
fn test_ai_udf_cycle_leaves_lalm_and_zeroes_afvl() {
    // C aiRecord.c:319-323: checkAlarms returns immediately on a UDF cycle —
    // it raises UDF_ALARM/UDFS, sets AFVL=0, and never runs the range check,
    // so LALM is left at its previous value. The same guard appears in
    // ao/longin/longout/int64in/int64out/calc/calcout checkAlarms; the port
    // ran the range check unconditionally, drifting LALM to val (NaN on an
    // undefined cycle) and filtering AFVL.
    let mut rec = AiRecord::default();
    rec.aftc = 10.0; // AFTC-capable filter enabled (ai carries AFVL)
    let mut instance = RecordInstance::new("TEST".into(), rec);
    instance.common.analog_alarm = Some(AnalogAlarmConfig {
        hihi: AlarmLimit::Double(1.0),
        high: AlarmLimit::Double(0.5),
        low: AlarmLimit::Double(-1000.0),
        lolo: AlarmLimit::Double(-2000.0),
        hhsv: AlarmSeverity::Major as i16,
        hsv: AlarmSeverity::Major as i16,
        lsv: AlarmSeverity::Minor as i16,
        llsv: AlarmSeverity::Major as i16,
    });
    // Seed LALM/AFVL to sentinels, then force a UDF cycle.
    instance
        .record
        .put_field("LALM", EpicsValue::Double(0.5))
        .unwrap();
    instance
        .record
        .put_field("AFVL", EpicsValue::Double(3.0))
        .unwrap();
    instance
        .record
        .set_val(EpicsValue::Double(f64::NAN))
        .unwrap();
    instance.common.udf = 1;

    instance.evaluate_alarms();

    assert_eq!(
        instance.record.get_field("LALM").and_then(|v| v.to_f64()),
        Some(0.5),
        "UDF cycle must leave LALM untouched (C returns before the range check)"
    );
    assert_eq!(
        instance.record.get_field("AFVL").and_then(|v| v.to_f64()),
        Some(0.0),
        "UDF cycle zeroes AFVL (aiRecord.c:321)"
    );
    assert_eq!(
        instance.common.nsev,
        AlarmSeverity::Invalid,
        "UDF cycle raises UDFS severity (default INVALID)"
    );
}

#[test]
fn test_waveform_onchange_gates_val_and_posts_hash() {
    // C waveform monitor() (waveformRecord.c:298-324): in On Change mode the
    // VAL post (DBE_VALUE/DBE_LOG) and the HASH post (literal DBE_VALUE) fire
    // only when the array-content hash differs from the stored HASH.
    use epics_base_rs::server::recgbl::EventMask;
    use epics_base_rs::server::records::waveform::WaveformRecord;
    use epics_base_rs::types::DbFieldType;
    let mut rec = WaveformRecord::new(10, DbFieldType::Long);
    rec.mpst = 1; // On Change
    rec.apst = 1; // On Change
    let mut instance = RecordInstance::new("WF".into(), rec);
    let _val_rx = instance
        .add_subscriber("VAL", 1, DbFieldType::Long, EventMask::VALUE.bits())
        .expect("VAL subscriber");
    let _hash_rx = instance
        .add_subscriber("HASH", 2, DbFieldType::ULong, EventMask::VALUE.bits())
        .expect("HASH subscriber");

    // Cycle 1: write [1,2,3] → hash changes → VAL and HASH post.
    instance
        .record
        .put_field("VAL", EpicsValue::LongArray(vec![1, 2, 3]))
        .unwrap();
    let (snap, _) = instance.process_local().unwrap();
    assert!(
        snap.changed_fields.iter().any(|(k, _, _)| k == "VAL"),
        "On Change: VAL posts when content hash changes"
    );
    let hash_post = snap.changed_fields.iter().find(|(k, _, _)| k == "HASH");
    assert!(hash_post.is_some(), "HASH posts on a hash change");
    assert_eq!(
        hash_post.unwrap().2,
        EventMask::VALUE,
        "HASH posts with a literal DBE_VALUE (waveformRecord.c:319)"
    );
    // HASH == epicsMemHash over the first NORD=3 i32 LE elements [1,2,3].
    assert_eq!(
        instance.record.get_field("HASH"),
        Some(EpicsValue::ULong(0x3429_76d1)),
        "HASH equals epicsMemHash(i32[1,2,3] LE)"
    );

    // Cycle 2: same content → hash unchanged → neither VAL nor HASH post.
    instance
        .record
        .put_field("VAL", EpicsValue::LongArray(vec![1, 2, 3]))
        .unwrap();
    let (snap, _) = instance.process_local().unwrap();
    assert!(
        !snap.changed_fields.iter().any(|(k, _, _)| k == "VAL"),
        "On Change: VAL suppressed when content hash is unchanged"
    );
    assert!(
        !snap.changed_fields.iter().any(|(k, _, _)| k == "HASH"),
        "HASH not posted when the hash is unchanged"
    );

    // Cycle 3: new content → hash changes → VAL and HASH post again.
    instance
        .record
        .put_field("VAL", EpicsValue::LongArray(vec![1, 2, 4]))
        .unwrap();
    let (snap, _) = instance.process_local().unwrap();
    assert!(
        snap.changed_fields.iter().any(|(k, _, _)| k == "VAL"),
        "On Change: VAL posts again on new content"
    );
    assert!(
        snap.changed_fields.iter().any(|(k, _, _)| k == "HASH"),
        "HASH posts again on new content"
    );
}

#[test]
fn test_waveform_always_mode_never_posts_hash() {
    // C waveform monitor() in the default Always mode (mpst=apst=0) never
    // enters the hash block: HASH is neither computed nor posted, and VAL
    // posts every cycle with DBE_VALUE|DBE_LOG. A HASH subscriber must see
    // no process-driven HASH event.
    use epics_base_rs::server::recgbl::EventMask;
    use epics_base_rs::server::records::waveform::WaveformRecord;
    use epics_base_rs::types::DbFieldType;
    // mpst=apst=0 (Always) by default.
    let rec = WaveformRecord::new(10, DbFieldType::Long);
    let mut instance = RecordInstance::new("WF".into(), rec);
    let _hash_rx = instance
        .add_subscriber("HASH", 1, DbFieldType::ULong, EventMask::VALUE.bits())
        .expect("HASH subscriber");

    instance
        .record
        .put_field("VAL", EpicsValue::LongArray(vec![1, 2, 3]))
        .unwrap();
    let (snap, _) = instance.process_local().unwrap();
    assert!(
        !snap.changed_fields.iter().any(|(k, _, _)| k == "HASH"),
        "Always mode: HASH is never posted (C never enters the hash block)"
    );
    assert_eq!(
        instance.record.get_field("HASH"),
        Some(EpicsValue::ULong(0)),
        "Always mode: HASH stays 0 (never computed)"
    );
}

#[test]
fn test_pact_reads_zero_when_idle() {
    let instance = RecordInstance::new("TEST".into(), AoRecord::new(0.0));
    match instance.get_common_field("PACT") {
        Some(EpicsValue::Char(0)) => {}
        other => panic!("expected Char(0), got {:?}", other),
    }
}

#[test]
fn test_pact_write_rejected() {
    let mut instance = RecordInstance::new("TEST".into(), AoRecord::new(0.0));
    let result = instance.put_common_field("PACT", EpicsValue::Char(1));
    assert!(matches!(result, Err(CaError::ReadOnlyField(_))));
}

#[test]
fn test_lcnt_zero_after_process() {
    let mut instance = RecordInstance::new("TEST".into(), AoRecord::new(0.0));
    instance.common.lcnt = 5;
    let _ = instance.process_local().unwrap();
    assert_eq!(instance.common.lcnt, 0);
}

#[test]
fn test_lcnt_increments_on_reentrance() {
    let mut instance = RecordInstance::new("TEST".into(), AoRecord::new(0.0));
    instance.enter_pact();
    let _ = instance.process_local().unwrap();
    assert_eq!(instance.common.lcnt, 1);
    let _ = instance.process_local().unwrap();
    assert_eq!(instance.common.lcnt, 2);
}

#[test]
fn test_lcnt_alarm_threshold() {
    // C `dbProcess` (dbAccess.c:543-556): the SCAN_ALARM raise fires on
    // the attempt whose PRE-increment lcnt equals MAX_LOCK=10 — i.e.
    // the 11th consecutive reentrant attempt, not the 10th.
    let mut instance = RecordInstance::new("TEST".into(), AoRecord::new(0.0));
    instance.enter_pact();
    for _ in 0..10 {
        let _ = instance.process_local().unwrap();
    }
    assert_eq!(instance.common.lcnt, 10);
    assert_eq!(
        instance.common.sevr,
        AlarmSeverity::NoAlarm,
        "C fires at lcnt++ == MAX_LOCK: 10 attempts only increment"
    );
    let _ = instance.process_local().unwrap();
    assert_eq!(instance.common.sevr, AlarmSeverity::Invalid);
    // C `menuAlarmStat.dbd`: SCAN_ALARM = 13.
    assert_eq!(
        instance.common.stat,
        epics_base_rs::server::recgbl::alarm_status::SCAN_ALARM
    );
    // C `recGblSetSevrMsg(..., "Async in progress")` — the alarm text
    // lands in AMSG via the reset (epics-base PR #568).
    assert_eq!(instance.common.amsg, "Async in progress");
}

#[test]
fn test_lcnt_alarm_posts_exactly_once() {
    // C posts the SCAN_ALARM transition exactly once: subsequent
    // reentrant attempts bail on `stat == SCAN_ALARM` / `sevr >=
    // INVALID_ALARM` (dbAccess.c:544-546). The pre-fix guard re-posted
    // the unchanged SEVR/STAT/VAL on every attempt past the threshold.
    let mut instance = RecordInstance::new("TEST".into(), AoRecord::new(0.0));
    instance.enter_pact();
    for _ in 0..10 {
        let _ = instance.process_local().unwrap();
    }
    // 11th attempt: fresh raise — snapshot carries the transition.
    let (snapshot, _) = instance.process_local().unwrap();
    assert!(
        snapshot.changed_fields.iter().any(|(f, _, _)| f == "SEVR"),
        "the fresh SCAN_ALARM raise must post SEVR"
    );
    // 12th and later attempts: already raised — nothing re-posts.
    let (snapshot, _) = instance.process_local().unwrap();
    assert!(
        snapshot.changed_fields.is_empty(),
        "an already-raised SCAN_ALARM must not re-post on later \
         reentrant attempts, got {:?}",
        snapshot.changed_fields
    );
}

#[test]
fn test_lcnt_reset_on_success() {
    let mut instance = RecordInstance::new("TEST".into(), AoRecord::new(0.0));
    instance.common.lcnt = 5;
    let _ = instance.process_local().unwrap();
    assert_eq!(instance.common.lcnt, 0);
}

#[test]
fn test_proc_get_put() {
    // C `dbCommon.dbd`: `field(PROC,DBF_UCHAR)` — the raw put byte is retained
    // in `prec->proc` and served back as `DBF_UCHAR` (`UChar`), like DISP. A
    // fresh record reads the default 0; a stored byte round-trips. (The
    // `pp(TRUE)` force-process is orthogonal and driven by the put path.)
    let mut instance = RecordInstance::new("TEST".into(), AoRecord::new(0.0));
    match instance.get_common_field("PROC") {
        Some(EpicsValue::UChar(0)) => {}
        other => panic!("expected UChar(0), got {:?}", other),
    }
    instance
        .put_common_field("PROC", EpicsValue::Char(1))
        .unwrap();
    assert_eq!(instance.common.proc_field, 1);
    match instance.get_common_field("PROC") {
        Some(EpicsValue::UChar(1)) => {}
        other => panic!("expected UChar(1), got {:?}", other),
    }
}

#[test]
fn test_disp_get_put() {
    let mut instance = RecordInstance::new("TEST".into(), AoRecord::new(0.0));
    // DISP is `DBF_UCHAR`: served as `UChar` (its declared type).
    match instance.get_common_field("DISP") {
        Some(EpicsValue::UChar(0)) => {}
        other => panic!("expected UChar(0), got {:?}", other),
    }
    instance
        .put_common_field("DISP", EpicsValue::Char(1))
        .unwrap();
    assert!(instance.common.disp != 0);
    match instance.get_common_field("DISP") {
        Some(EpicsValue::UChar(1)) => {}
        other => panic!("expected UChar(1), got {:?}", other),
    }
}

// --- Hook Framework tests ---

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

struct HookTrackingRecord {
    val: f64,
    special_before_count: Arc<AtomicU32>,
    special_after_count: Arc<AtomicU32>,
    on_put_count: Arc<AtomicU32>,
    reject_field: Option<String>,
}

impl Record for HookTrackingRecord {
    fn record_type(&self) -> &'static str {
        "test_hook"
    }
    fn get_field(&self, name: &str) -> Option<EpicsValue> {
        match name {
            "VAL" => Some(EpicsValue::Double(self.val)),
            _ => None,
        }
    }
    fn put_field(&mut self, name: &str, value: EpicsValue) -> CaResult<()> {
        match name {
            "VAL" => {
                if let EpicsValue::Double(v) = value {
                    self.val = v;
                    Ok(())
                } else {
                    Err(CaError::InvalidValue("bad type".into()))
                }
            }
            _ => Err(CaError::FieldNotFound(name.into())),
        }
    }
    fn declared_fields(&self) -> &'static [FieldDesc] {
        static FIELDS: &[FieldDesc] = &[FieldDesc::new("VAL", DbFieldType::Double, false)];
        FIELDS
    }
    fn validate_put(&self, field: &str, _value: &EpicsValue) -> CaResult<()> {
        if let Some(ref reject) = self.reject_field {
            if field == reject {
                return Err(CaError::InvalidValue("rejected by validate_put".into()));
            }
        }
        Ok(())
    }
    fn on_put(&mut self, _field: &str) {
        self.on_put_count.fetch_add(1, Ordering::SeqCst);
    }
    fn special(&mut self, _field: &str, after: bool) -> CaResult<()> {
        if after {
            self.special_after_count.fetch_add(1, Ordering::SeqCst);
        } else {
            self.special_before_count.fetch_add(1, Ordering::SeqCst);
        }
        Ok(())
    }
}

#[test]
fn test_special_called_on_common_put() {
    let special_before = Arc::new(AtomicU32::new(0));
    let special_after = Arc::new(AtomicU32::new(0));
    let rec = HookTrackingRecord {
        val: 0.0,
        special_before_count: special_before.clone(),
        special_after_count: special_after.clone(),
        on_put_count: Arc::new(AtomicU32::new(0)),
        reject_field: None,
    };
    let mut instance = RecordInstance::new("TEST".into(), rec);
    instance
        .put_common_field("DESC", EpicsValue::String("hello".into()))
        .unwrap();
    assert_eq!(special_before.load(Ordering::SeqCst), 1);
    assert_eq!(special_after.load(Ordering::SeqCst), 1);
}

#[test]
fn test_validate_put_rejects_common_field() {
    let rec = HookTrackingRecord {
        val: 0.0,
        special_before_count: Arc::new(AtomicU32::new(0)),
        special_after_count: Arc::new(AtomicU32::new(0)),
        on_put_count: Arc::new(AtomicU32::new(0)),
        reject_field: Some("SCAN".into()),
    };
    let mut instance = RecordInstance::new("TEST".into(), rec);
    let result = instance.put_common_field("SCAN", EpicsValue::String("1 second".into()));
    assert!(result.is_err());
}

#[test]
fn test_on_put_called_for_common_field() {
    let on_put = Arc::new(AtomicU32::new(0));
    let rec = HookTrackingRecord {
        val: 0.0,
        special_before_count: Arc::new(AtomicU32::new(0)),
        special_after_count: Arc::new(AtomicU32::new(0)),
        on_put_count: on_put.clone(),
        reject_field: None,
    };
    let mut instance = RecordInstance::new("TEST".into(), rec);
    instance
        .put_common_field("DESC", EpicsValue::String("test".into()))
        .unwrap();
    assert_eq!(on_put.load(Ordering::SeqCst), 1);
}

// --- Scan Index tests ---

#[test]
fn test_phas_change_returns_result() {
    let rec = AiRecord::new(0.0);
    let mut instance = RecordInstance::new("TEST".into(), rec);
    instance
        .put_common_field("SCAN", EpicsValue::String("1 second".into()))
        .unwrap();
    let result = instance
        .put_common_field("PHAS", EpicsValue::Short(5))
        .unwrap();
    assert!(matches!(
        result,
        CommonFieldPutResult::PhasChanged {
            old_phas: 0,
            new_phas: 5,
            ..
        }
    ));
}

#[test]
fn test_phas_change_passive_no_result() {
    let rec = AiRecord::new(0.0);
    let mut instance = RecordInstance::new("TEST".into(), rec);
    let result = instance
        .put_common_field("PHAS", EpicsValue::Short(5))
        .unwrap();
    assert_eq!(result, CommonFieldPutResult::NoChange);
}

#[test]
fn test_scan_change_includes_phas() {
    let rec = AiRecord::new(0.0);
    let mut instance = RecordInstance::new("TEST".into(), rec);
    instance
        .put_common_field("PHAS", EpicsValue::Short(3))
        .unwrap();
    let result = instance
        .put_common_field("SCAN", EpicsValue::String("1 second".into()))
        .unwrap();
    match result {
        CommonFieldPutResult::ScanChanged { phas, .. } => assert_eq!(phas, 3),
        other => panic!("expected ScanChanged, got {:?}", other),
    }
}

// --- UDF Policy tests ---

struct NoUdfClearRecord {
    val: f64,
}
impl Record for NoUdfClearRecord {
    fn record_type(&self) -> &'static str {
        "test_noudf"
    }
    fn get_field(&self, name: &str) -> Option<EpicsValue> {
        match name {
            "VAL" => Some(EpicsValue::Double(self.val)),
            _ => None,
        }
    }
    fn put_field(&mut self, name: &str, value: EpicsValue) -> CaResult<()> {
        match name {
            "VAL" => {
                if let EpicsValue::Double(v) = value {
                    self.val = v;
                    Ok(())
                } else {
                    Err(CaError::InvalidValue("bad".into()))
                }
            }
            _ => Err(CaError::FieldNotFound(name.into())),
        }
    }
    fn declared_fields(&self) -> &'static [FieldDesc] {
        &[]
    }
    fn clears_udf(&self) -> bool {
        false
    }
}

#[test]
fn test_udf_cleared_after_process() {
    let rec = AiRecord::new(1.0);
    let mut instance = RecordInstance::new("TEST".into(), rec);
    assert!(instance.common.udf != 0);
    instance.process_local().unwrap();
    assert!(instance.common.udf == 0);
}

#[test]
fn test_udf_not_cleared_when_clears_udf_false() {
    let rec = NoUdfClearRecord { val: 1.0 };
    let mut instance = RecordInstance::new("TEST".into(), rec);
    assert!(instance.common.udf != 0);
    instance.process_local().unwrap();
    assert!(instance.common.udf != 0);
}

#[test]
fn test_udf_alarm_persists() {
    use epics_base_rs::server::recgbl;
    let rec = NoUdfClearRecord { val: 1.0 };
    let mut instance = RecordInstance::new("TEST".into(), rec);
    instance.common.udf = 1;
    instance.process_local().unwrap();
    assert!(instance.common.udf != 0);
    instance.evaluate_alarms();
    let result = recgbl::rec_gbl_reset_alarms(&mut instance.common);
    assert!(result.alarm_changed || instance.common.sevr == AlarmSeverity::Invalid);
}

// ---- Snapshot generation tests ----

#[test]
fn test_snapshot_ai_with_display_metadata() {
    let mut rec = AiRecord::new(42.0);
    rec.egu = "degC".into();
    rec.prec = 3;
    rec.hopr = 100.0;
    rec.lopr = -50.0;
    let mut inst = RecordInstance::new("AI:TEST".into(), rec);
    inst.common.analog_alarm = Some(AnalogAlarmConfig {
        hihi: AlarmLimit::Double(90.0),
        high: AlarmLimit::Double(80.0),
        low: AlarmLimit::Double(-20.0),
        lolo: AlarmLimit::Double(-40.0),
        hhsv: AlarmSeverity::Major as i16,
        hsv: AlarmSeverity::Minor as i16,
        lsv: AlarmSeverity::Minor as i16,
        llsv: AlarmSeverity::Major as i16,
    });

    let snap = inst.snapshot_for_field("VAL").unwrap();
    assert_eq!(snap.value, EpicsValue::Double(42.0));
    let disp = snap.display.as_ref().unwrap();
    assert_eq!(disp.units, "degC");
    assert_eq!(disp.precision, 3);
    assert_eq!(disp.upper_disp_limit, 100.0);
    assert_eq!(disp.lower_disp_limit, -50.0);
    assert_eq!(disp.upper_alarm_limit, 90.0);
    assert_eq!(disp.upper_warning_limit, 80.0);
    assert_eq!(disp.lower_warning_limit, -20.0);
    assert_eq!(disp.lower_alarm_limit, -40.0);
    let ctrl = snap.control.as_ref().unwrap();
    assert_eq!(ctrl.upper_ctrl_limit, 100.0);
    assert_eq!(ctrl.lower_ctrl_limit, -50.0);
    assert!(snap.enums.is_none());
}

#[test]
fn test_snapshot_ao_with_drvh_drvl() {
    let mut rec = AoRecord::new(10.0);
    rec.egu = "V".into();
    rec.hopr = 100.0;
    rec.lopr = 0.0;
    rec.drvh = 50.0;
    rec.drvl = 5.0;
    let inst = RecordInstance::new("AO:TEST".into(), rec);

    let snap = inst.snapshot_for_field("VAL").unwrap();
    let ctrl = snap.control.as_ref().unwrap();
    assert_eq!(ctrl.upper_ctrl_limit, 50.0);
    assert_eq!(ctrl.lower_ctrl_limit, 5.0);
    let disp = snap.display.as_ref().unwrap();
    assert_eq!(disp.units, "V");
}

#[test]
fn test_snapshot_bi_enum_strings() {
    let mut rec = BiRecord::new(0);
    rec.znam = "Off".into();
    rec.onam = "On".into();
    let inst = RecordInstance::new("BI:TEST".into(), rec);

    let snap = inst.snapshot_for_field("VAL").unwrap();
    // bi has no display source beyond dbCommon DESC (UI-106): the block
    // exists solely to carry `description`, everything else default.
    let disp = snap.display.as_ref().unwrap();
    assert!(disp.units.is_empty());
    assert!(disp.description.is_empty());
    assert!(snap.control.is_none());
    let enums = snap.enums.as_ref().unwrap();
    assert_eq!(enums.strings.len(), 2);
    assert_eq!(enums.strings[0], "Off");
    assert_eq!(enums.strings[1], "On");
}

#[test]
fn test_snapshot_mbbi_16_strings() {
    use epics_base_rs::server::records::mbbi::MbbiRecord;
    let mut rec = MbbiRecord::default();
    rec.zrst = "Zero".into();
    rec.onst = "One".into();
    rec.twst = "Two".into();
    rec.ffst = "Fifteen".into();
    let inst = RecordInstance::new("MBBI:TEST".into(), rec);

    let snap = inst.snapshot_for_field("VAL").unwrap();
    let enums = snap.enums.as_ref().unwrap();
    assert_eq!(enums.strings.len(), 16);
    assert_eq!(enums.strings[0], "Zero");
    assert_eq!(enums.strings[1], "One");
    assert_eq!(enums.strings[2], "Two");
    assert_eq!(enums.strings[15], "Fifteen");
    assert_eq!(enums.strings[3], "");
}

// A record-specific DBF_MENU field (sel.SELM, menu(selSELM)
// selRecord.dbd.pod:21-26) is served as DBR_ENUM: the field snapshot
// carries the menu index as Enum and the menu's choice labels, so
// caget/pvget present "Low Signal" instead of a bare 2.
#[test]
fn test_snapshot_sel_selm_menu_choices() {
    use epics_base_rs::server::records::sel::SelRecord;
    let mut rec = SelRecord::default();
    rec.selm = 2; // Low Signal
    let inst = RecordInstance::new("SEL:TEST".into(), rec);

    let snap = inst.snapshot_for_field("SELM").unwrap();
    assert_eq!(snap.value, EpicsValue::Enum(2));
    let enums = snap.enums.as_ref().unwrap();
    assert_eq!(
        enums.strings,
        vec!["Specified", "High Signal", "Low Signal", "Median Signal"]
    );
}

// SIMM is DBF_MENU, but its menu differs by record family: menu(menuSimm)
// (NO/YES/RAW) on the analog/binary/multibit records, menu(menuYesNo)
// (NO/YES) on the integer/string/long records. The snapshot boundary must
// serve each record its OWN choice table — the field name alone is not
// enough. These two cases pin both halves of that split.
#[test]
fn test_snapshot_ai_simm_is_menusimm_three_choices() {
    let mut rec = AiRecord::new(1.0);
    rec.simm = 2; // RAW — only present in menuSimm
    let inst = RecordInstance::new("AI:SIMM".into(), rec);

    let snap = inst.snapshot_for_field("SIMM").unwrap();
    assert_eq!(snap.value, EpicsValue::Enum(2));
    assert_eq!(
        snap.enums.as_ref().unwrap().strings,
        vec!["NO", "YES", "RAW"]
    );
}

#[test]
fn test_snapshot_longout_simm_is_menuyesno_two_choices() {
    use epics_base_rs::server::records::longout::LongoutRecord;
    let mut rec = LongoutRecord::new(0);
    rec.simm = 1; // YES
    let inst = RecordInstance::new("LO:SIMM".into(), rec);

    let snap = inst.snapshot_for_field("SIMM").unwrap();
    assert_eq!(snap.value, EpicsValue::Enum(1));
    assert_eq!(snap.enums.as_ref().unwrap().strings, vec!["NO", "YES"]);
}

// The promotion lives at the snapshot boundary ONLY: get_field keeps
// returning Short so record-internal callers (processing, alarm logic that
// match EpicsValue::Short on SIMM/OMSL/IVOA) are unchanged.
#[test]
fn test_ai_simm_get_field_stays_short() {
    let mut rec = AiRecord::new(1.0);
    rec.simm = 2;
    assert_eq!(rec.get_field("SIMM"), Some(EpicsValue::Short(2)));
}

// dfanout OMSL/IVOA are DBF_MENU (menuOmsl / menuIvoa), so
// the snapshot boundary must serve them as DBR_ENUM with the menu labels,
// not a bare DBR_SHORT. dfanout's own menu_field_choices overrides only
// SELM, so OMSL/IVOA fall through to the shared (field-name) registry. A
// sub-agent reported them served as SHORT; this pins the ENUM form (already
// produced since the menu-serving owner landed) so the finding stays closed.
#[test]
fn test_snapshot_dfanout_omsl_ivoa_serve_as_enum() {
    use epics_base_rs::server::records::dfanout::DfanoutRecord;
    let mut rec = DfanoutRecord::new(0.0);
    rec.omsl = 1; // closed_loop
    rec.ivoa = 2; // Set output to IVOV
    // Promotion is boundary-only: the raw record keeps Short so internal
    // OMSL/IVOA match arms are unaffected.
    assert_eq!(rec.get_field("OMSL"), Some(EpicsValue::Short(1)));
    assert_eq!(rec.get_field("IVOA"), Some(EpicsValue::Short(2)));
    let inst = RecordInstance::new("DFAN:MENU".into(), rec);

    let omsl = inst.snapshot_for_field("OMSL").unwrap();
    assert_eq!(omsl.value, EpicsValue::Enum(1));
    assert_eq!(
        omsl.enums.as_ref().unwrap().strings,
        vec!["supervisory", "closed_loop"]
    );

    let ivoa = inst.snapshot_for_field("IVOA").unwrap();
    assert_eq!(ivoa.value, EpicsValue::Enum(2));
    assert_eq!(
        ivoa.enums.as_ref().unwrap().strings,
        vec![
            "Continue normally",
            "Don't drive outputs",
            "Set output to IVOV"
        ]
    );
}

// R6-1 — every dbCommon DBF_MENU field is served as DBR_ENUM with its menu's
// choice strings, not as a bare SHORT/CHAR. C `dbAccess.c:89`
// (`mapDBFToDBR[DBF_MENU] == DBR_ENUM`) + `:167-175` (`get_enum_strs` serves
// the menu's `papChoiceValue[]`). Pre-fix `caget REC.SEVR` returned DBR_SHORT
// 2 with no strings where a C IOC returns DBR_ENUM 2 + "MAJOR".
#[test]
fn test_snapshot_dbcommon_menu_fields_serve_as_enum() {
    use epics_base_rs::server::record::{AlarmSeverity, PiniMode};

    let mut inst = RecordInstance::new("COMMON:MENU".into(), AiRecord::new(1.0));
    inst.common.sevr = AlarmSeverity::Major; // menuAlarmSevr index 2
    inst.common.stat = 14; // menuAlarmStat LINK
    inst.common.nsev = AlarmSeverity::Minor;
    inst.common.nsta = 17; // UDF
    inst.common.acks = AlarmSeverity::Invalid;
    inst.common.ackt = false;
    inst.common.diss = AlarmSeverity::Minor as i16;
    inst.common.udfs = AlarmSeverity::Invalid as i16;
    inst.common.pini = PiniMode::Run as i16;

    let sevr = inst.snapshot_for_field("SEVR").unwrap();
    assert_eq!(sevr.value, EpicsValue::Enum(2));
    assert_eq!(
        sevr.enums.as_ref().unwrap().strings,
        vec!["NO_ALARM", "MINOR", "MAJOR", "INVALID"]
    );

    let stat = inst.snapshot_for_field("STAT").unwrap();
    assert_eq!(stat.value, EpicsValue::Enum(14));
    let stat_strings = &stat.enums.as_ref().unwrap().strings;
    assert_eq!(stat_strings.len(), 22);
    assert_eq!(stat_strings[14], "LINK");

    let nsev = inst.snapshot_for_field("NSEV").unwrap();
    assert_eq!(nsev.value, EpicsValue::Enum(1));
    let nsta = inst.snapshot_for_field("NSTA").unwrap();
    assert_eq!(nsta.value, EpicsValue::Enum(17));
    assert_eq!(nsta.enums.as_ref().unwrap().strings[17], "UDF");

    let acks = inst.snapshot_for_field("ACKS").unwrap();
    assert_eq!(acks.value, EpicsValue::Enum(3));
    assert_eq!(acks.enums.as_ref().unwrap().strings[3], "INVALID");

    // ACKT/PINI were served as DBR_CHAR (a 1-byte payload) where C sends a
    // 2-byte DBR_ENUM.
    let ackt = inst.snapshot_for_field("ACKT").unwrap();
    assert_eq!(ackt.value, EpicsValue::Enum(0));
    assert_eq!(ackt.enums.as_ref().unwrap().strings, vec!["NO", "YES"]);

    let pini = inst.snapshot_for_field("PINI").unwrap();
    assert_eq!(pini.value, EpicsValue::Enum(2));
    assert_eq!(
        pini.enums.as_ref().unwrap().strings,
        vec!["NO", "YES", "RUN", "RUNNING", "PAUSE", "PAUSED"]
    );

    let diss = inst.snapshot_for_field("DISS").unwrap();
    assert_eq!(diss.value, EpicsValue::Enum(1));
    assert_eq!(diss.enums.as_ref().unwrap().strings[1], "MINOR");

    let udfs = inst.snapshot_for_field("UDFS").unwrap();
    assert_eq!(udfs.value, EpicsValue::Enum(3));
    assert_eq!(udfs.enums.as_ref().unwrap().strings[3], "INVALID");
}

// R6-1 (put half) — a DBR_STRING / `.db` write to a dbCommon DBF_MENU field
// resolves the label against THAT field's own menu (C `dbPutStringNum`: exact
// label, then a numeric index). Pre-fix `field(DISS,"MAJOR")` was dropped
// silently (the arm bound only `Short`) and `field(PINI,"RUN")` set the flag
// to false.
#[test]
fn test_put_dbcommon_menu_field_resolves_label() {
    use epics_base_rs::server::record::{AlarmSeverity, PiniMode};

    let mut inst = RecordInstance::new("COMMON:PUT".into(), AiRecord::new(1.0));
    inst.put_common_field("DISS", EpicsValue::String("MAJOR".into()))
        .unwrap();
    assert_eq!(inst.common.diss, AlarmSeverity::Major as i16);

    inst.put_common_field("UDFS", EpicsValue::String("MINOR".into()))
        .unwrap();
    assert_eq!(inst.common.udfs, AlarmSeverity::Minor as i16);

    inst.put_common_field("PINI", EpicsValue::String("RUN".into()))
        .unwrap();
    assert_eq!(inst.common.pini, PiniMode::Run as i16);
    // Bare menu index, as C `epicsParseUInt16` accepts.
    inst.put_common_field("PINI", EpicsValue::String("3".into()))
        .unwrap();
    assert_eq!(inst.common.pini, PiniMode::Running as i16);
    // "NO" is a real choice — index 0, not a parse failure.
    inst.put_common_field("PINI", EpicsValue::String("NO".into()))
        .unwrap();
    assert_eq!(inst.common.pini, PiniMode::No as i16);
    // A string naming no choice is C `S_db_badChoice`, not a silent NO.
    assert!(
        inst.put_common_field("PINI", EpicsValue::String("MAYBE".into()))
            .is_err()
    );

    // ACKT is menu(menuYesNo), not a truthiness flag.
    inst.put_common_field("ACKT", EpicsValue::String("NO".into()))
        .unwrap();
    assert!(!inst.common.ackt);
    inst.put_common_field("ACKT", EpicsValue::String("YES".into()))
        .unwrap();
    assert!(inst.common.ackt);
}

// MPST/APST on lsi are menu(menuPost): On Change (0), Always (1). The value
// order is wire-visible; the array records' POST menus reverse it, which is
// why MPST/APST are resolved per record rather than globally.
#[test]
fn test_snapshot_lsi_mpst_menupost_order() {
    use epics_base_rs::server::records::lsi::LsiRecord;
    let mut rec = LsiRecord::new("x");
    rec.mpst = 1; // Always
    let inst = RecordInstance::new("LSI:MPST".into(), rec);

    let snap = inst.snapshot_for_field("MPST").unwrap();
    assert_eq!(snap.value, EpicsValue::Enum(1));
    assert_eq!(
        snap.enums.as_ref().unwrap().strings,
        vec!["On Change", "Always"]
    );
}

// A field whose name collides with a shared menu but is served centrally:
// SIMS is menu(menuAlarmSevr) on every record, resolved by the global
// registry without a per-record override.
#[test]
fn test_snapshot_ai_sims_alarm_severity_choices() {
    let mut rec = AiRecord::new(1.0);
    rec.sims = 3; // INVALID
    let inst = RecordInstance::new("AI:SIMS".into(), rec);

    let snap = inst.snapshot_for_field("SIMS").unwrap();
    assert_eq!(snap.value, EpicsValue::Enum(3));
    assert_eq!(
        snap.enums.as_ref().unwrap().strings,
        vec!["NO_ALARM", "MINOR", "MAJOR", "INVALID"]
    );
}

// Record-specific output-policy menus carry their own labels in .dbd order.
#[test]
fn test_snapshot_ao_oif_menu_choices() {
    let mut rec = AoRecord::new(0.0);
    rec.oif = 1; // Incremental
    let inst = RecordInstance::new("AO:OIF".into(), rec);

    let snap = inst.snapshot_for_field("OIF").unwrap();
    assert_eq!(snap.value, EpicsValue::Enum(1));
    assert_eq!(
        snap.enums.as_ref().unwrap().strings,
        vec!["Full", "Incremental"]
    );
}

#[test]
fn test_snapshot_histogram_cmd_menu_choices() {
    use epics_base_rs::server::records::histogram::HistogramRecord;
    let mut rec = HistogramRecord::default();
    rec.cmd = 2; // Start
    let inst = RecordInstance::new("HIST:CMD".into(), rec);

    let snap = inst.snapshot_for_field("CMD").unwrap();
    assert_eq!(snap.value, EpicsValue::Enum(2));
    assert_eq!(
        snap.enums.as_ref().unwrap().strings,
        vec!["Read", "Clear", "Start", "Stop"]
    );
}

// scalcoutOOPT extends the six longoutOOPT choices with a trailing "Never"
// (index 6) — the wire-visible label set must include it.
#[test]
fn test_snapshot_scalcout_oopt_includes_never() {
    use epics_base_rs::server::records::scalcout::ScalcoutRecord;
    let mut rec = ScalcoutRecord::default();
    rec.oopt = 6; // Never
    let inst = RecordInstance::new("SCALC:OOPT".into(), rec);

    let snap = inst.snapshot_for_field("OOPT").unwrap();
    assert_eq!(snap.value, EpicsValue::Enum(6));
    let strings = &snap.enums.as_ref().unwrap().strings;
    assert_eq!(strings.len(), 7);
    assert_eq!(strings[0], "Every Time");
    assert_eq!(strings[6], "Never");
}

#[test]
fn test_snapshot_longin_display() {
    use epics_base_rs::server::records::longin::LonginRecord;
    let mut rec = LonginRecord::new(999);
    rec.egu = "counts".into();
    rec.hopr = 10000;
    rec.lopr = 0;
    let inst = RecordInstance::new("LONGIN:TEST".into(), rec);

    let snap = inst.snapshot_for_field("VAL").unwrap();
    let disp = snap.display.as_ref().unwrap();
    assert_eq!(disp.units, "counts");
    assert_eq!(disp.precision, 0);
    assert_eq!(disp.upper_disp_limit, 10000.0);
    assert_eq!(disp.lower_disp_limit, 0.0);
    let ctrl = snap.control.as_ref().unwrap();
    assert_eq!(ctrl.upper_ctrl_limit, 10000.0);
    assert_eq!(ctrl.lower_ctrl_limit, 0.0);
}

#[test]
fn test_snapshot_stringin_no_metadata() {
    let rec = StringinRecord::new("hello");
    let inst = RecordInstance::new("SI:TEST".into(), rec);

    let snap = inst.snapshot_for_field("VAL").unwrap();
    assert_eq!(snap.value, EpicsValue::String("hello".into()));
    // stringin has no display source beyond dbCommon DESC (UI-106): the
    // block carries only `description`, everything else default.
    let disp = snap.display.as_ref().unwrap();
    assert!(disp.units.is_empty());
    assert_eq!(disp.upper_disp_limit, 0.0);
    assert!(snap.control.is_none());
    assert!(snap.enums.is_none());
}

#[test]
fn test_snapshot_field_not_found() {
    let rec = AiRecord::new(1.0);
    let inst = RecordInstance::new("AI:TEST".into(), rec);
    assert!(inst.snapshot_for_field("NONEXISTENT").is_none());
}

#[test]
fn test_snapshot_alarm_state() {
    let rec = AiRecord::new(1.0);
    let mut inst = RecordInstance::new("AI:TEST".into(), rec);
    inst.common.stat = 7;
    inst.common.sevr = AlarmSeverity::Minor;

    let snap = inst.snapshot_for_field("VAL").unwrap();
    assert_eq!(snap.alarm.status, 7);
    assert_eq!(snap.alarm.severity, 1);
}

// ---------------------------------------------------------------------------
// Alarm STATE/COS + AFTC-writeback regression tests for bi / mbbi.
//
// These pin two behaviours against the C sources:
//   (1) mbbi AFVL writeback — `mbbiRecord.c::checkAlarms` computes the
//       AFTC accumulator into a local; the Rust port persists it via
//       `record.put_field("AFVL", …)` each cycle.
//   (2) mbbi / bi LALM update around the COS_ALARM check
//       (`mbbiRecord.c:344-348`, `biRecord.c:276-278`).
//
// The shared AFTC alarm-range filter primitive itself is covered by the
// `records::alarm_filter` pure-function tests. `bi` gained the AFTC/AFVL
// alarm filter in EPICS PR #817 (`c9817fa59`); its line numbers below
// resolve at `678092d03`, five lines later. The bi-specific tests
// below pin (a) the fields being served, (b) the filter seeding AFVL, and
// (c) the parity distinction that — unlike mbbi/ai — the bi UDF path does
// NOT zero AFVL (biRecord.c:237-240 returns before `prec->afvl = afvl`).
// ---------------------------------------------------------------------------

/// `bi` serves AFTC as a settable `DBF_DOUBLE` and AFVL as a readable
/// `DBF_DOUBLE` (SPC_NOMOD). Before PR #817 the port had neither field, so
/// `caget ORACLE:BI.AFTC` / `.AFVL` timed out (the oracle's 22 ERRORs).
#[test]
fn test_bi_serves_aftc_and_afvl_fields() {
    use epics_base_rs::server::records::bi::BiRecord;

    let mut rec = BiRecord::new(0);
    // AFTC is settable by a client.
    rec.put_field("AFTC", EpicsValue::Double(5.0)).unwrap();
    assert_eq!(
        rec.get_field("AFTC").and_then(|v| v.to_f64()),
        Some(5.0),
        "AFTC must be a settable, readable DBF_DOUBLE"
    );
    // AFVL is readable (its SPC_NOMOD read-only-to-clients status is enforced
    // by the field table; the put arm exists for the framework filter owner).
    assert_eq!(
        rec.get_field("AFVL").and_then(|v| v.to_f64()),
        Some(0.0),
        "AFVL must be a readable DBF_DOUBLE, defaulting to 0"
    );
}

/// `bi` engages the alarm-range AFTC filter. On the first sample (AFVL == 0)
/// `biRecord.c:256-257` (at `678092d03`) seeds AFVL with the raw state severity and passes it
/// through, matching the shared `aftc_filter` seed branch. val==1 selects OSV.
#[test]
fn test_bi_aftc_filter_seeds_afvl() {
    use epics_base_rs::server::records::bi::BiRecord;

    let mut rec = BiRecord::new(1); // val=1 → OSV
    rec.osv = AlarmSeverity::Major as i16;
    rec.aftc = 5.0;
    rec.afvl = 0.0; // first sample
    let mut inst = RecordInstance::new("BI:AFTC".into(), rec);
    inst.common.udf = 0;

    inst.record.check_alarms(&mut inst.common);
    assert_eq!(
        inst.record.get_field("AFVL").and_then(|v| v.to_f64()),
        Some(AlarmSeverity::Major as u16 as f64),
        "first AFTC cycle seeds AFVL with the raw OSV severity (biRecord.c:256-257 at 678092d03)"
    );
}

/// Parity distinction from mbbi/ai: the `bi` UDF path raises UDF_ALARM and
/// returns WITHOUT touching AFVL — `biRecord.c:237-240` (at `678092d03`) returns before the
/// `prec->afvl = afvl` store, unlike `mbbiRecord.c`/`aiRecord.c` which zero it.
#[test]
fn test_bi_udf_cycle_leaves_afvl_untouched() {
    use epics_base_rs::server::records::bi::BiRecord;

    let mut rec = BiRecord::new(1);
    rec.aftc = 10.0;
    let mut inst = RecordInstance::new("BI:UDF".into(), rec);
    // Seed AFVL to a sentinel, then force a UDF cycle.
    inst.record
        .put_field("AFVL", EpicsValue::Double(3.0))
        .unwrap();
    inst.common.udf = 1;

    inst.record.check_alarms(&mut inst.common);
    assert_eq!(
        inst.record.get_field("AFVL").and_then(|v| v.to_f64()),
        Some(3.0),
        "bi UDF cycle must leave AFVL untouched (biRecord.c has no afvl=0 on the UDF path)"
    );
}

/// mbbi persists the AFTC accumulator back to AFVL each cycle.
/// `mbbiRecord.c::checkAlarms` (mbbiRecord.c:319-338) at the `R7.0.10`
/// pin computes the new accumulator into a local but — unlike
/// `aiRecord.c` etc. — never stores `prec->afvl = afvl`, so the pin's
/// mbbi filter is inert (it re-seeds from AFVL==0 every cycle). That is
/// the bug EPICS PR #817 `c9817fa59` fixed, adding the store at `:339`;
/// the Rust port routes through `record.put_field("AFVL", …)` after
/// `aftc_filter` and so implements the POST-fix C. Deviation from the
/// pin, documented at `MbbiRecord::check_alarms`. This test pins it.
#[test]
fn test_mbbi_aftc_writes_afvl_back_each_cycle() {
    use epics_base_rs::server::records::mbbi::MbbiRecord;

    let mut rec = MbbiRecord::new(1); // val=1 → ONSV
    rec.onsv = AlarmSeverity::Major as i16;
    rec.aftc = 2.0;
    rec.afvl = 0.0;

    let mut inst = RecordInstance::new("MBBI:AFTC".into(), rec);
    inst.common.udf = 0;

    // AFTC alarm filter runs inside `Record::check_alarms` (C
    // `mbbiRecord.c::checkAlarms`), the hook `process_local` invokes.
    inst.record.check_alarms(&mut inst.common);
    inst.evaluate_alarms();
    let afvl_after_first = inst
        .record
        .get_field("AFVL")
        .and_then(|v| v.to_f64())
        .expect("AFVL readable after first cycle");
    assert!(
        afvl_after_first != 0.0,
        "AFVL must be non-zero after first AFTC cycle (was the writeback dropped?)"
    );
    // Second cycle with the same val keeps the filter state alive
    // and yields a positive accumulator (steady-state aim is 2.0).
    inst.record.check_alarms(&mut inst.common);
    inst.evaluate_alarms();
    let afvl_after_second = inst
        .record
        .get_field("AFVL")
        .and_then(|v| v.to_f64())
        .expect("AFVL readable after second cycle");
    assert!(
        afvl_after_second.abs() > 0.0,
        "AFVL must remain non-zero after the second cycle"
    );
}

// ---------------------------------------------------------------------------
// Alarm-range AFTC filter parity for ai / longin / int64in.
//
// The 2009 EPICS Codeathon (epics-base `824d37811`) added the alarm
// filter to ai, calc, longin and mbbi (later int64in). The framework's
// `evaluate_analog_alarm` previously gated the filter to `calc` only;
// these records carry AFTC/AFVL in C but the gate ignored them. The fix
// adds the fields and broadens the gate to {calc, ai, longin, int64in}.
// ---------------------------------------------------------------------------

/// `ai` engages the alarm-range AFTC filter. aiRecord.c::checkAlarms:360-362
/// seeds AFVL with the raw `alarmRange` on the first sample (AFVL==0), so
/// a HIHI sample seeds AFVL=5.0 (range_Hihi) and passes MAJOR through.
/// Before the fix the filter never ran for `ai`, so AFVL stayed 0.
#[test]
fn test_ai_aftc_filter_engages_and_seeds() {
    let mut rec = AiRecord::new(150.0); // above HIHI
    rec.aftc = 5.0;
    rec.afvl = 0.0; // first sample
    let mut inst = RecordInstance::new("AI:AFTC".into(), rec);
    inst.common.udf = 0;
    inst.common.analog_alarm = Some(AnalogAlarmConfig {
        hihi: AlarmLimit::Double(100.0),
        high: AlarmLimit::Double(80.0),
        low: AlarmLimit::Double(-20.0),
        lolo: AlarmLimit::Double(-40.0),
        hhsv: AlarmSeverity::Major as i16,
        hsv: AlarmSeverity::Minor as i16,
        lsv: AlarmSeverity::Minor as i16,
        llsv: AlarmSeverity::Major as i16,
    });

    inst.evaluate_alarms();
    epics_base_rs::server::recgbl::rec_gbl_reset_alarms(&mut inst.common);

    assert_eq!(
        inst.common.sevr,
        AlarmSeverity::Major,
        "initial AFTC sample passes the raw HIHI severity through"
    );
    let afvl = inst
        .record
        .get_field("AFVL")
        .and_then(|v| v.to_f64())
        .expect("AFVL readable");
    assert!(
        (afvl - 5.0).abs() < 1e-9,
        "AFVL must seed to the raw HIHI alarmRange 5.0 (filter engaged), got {afvl}"
    );
}

/// `longin` engages the alarm-range AFTC filter (longinRecord.c:316-317
/// seed). A HIHI integer sample seeds AFVL=5.0 and reports MAJOR.
#[test]
fn test_longin_aftc_filter_engages_and_seeds() {
    use epics_base_rs::server::records::longin::LonginRecord;

    let mut rec = LonginRecord::new(150); // above HIHI
    rec.aftc = 5.0;
    rec.afvl = 0.0;
    let mut inst = RecordInstance::new("LONGIN:AFTC".into(), rec);
    inst.common.udf = 0;
    inst.common.analog_alarm = Some(AnalogAlarmConfig {
        hihi: AlarmLimit::Double(100.0),
        high: AlarmLimit::Double(80.0),
        low: AlarmLimit::Double(-20.0),
        lolo: AlarmLimit::Double(-40.0),
        hhsv: AlarmSeverity::Major as i16,
        hsv: AlarmSeverity::Minor as i16,
        lsv: AlarmSeverity::Minor as i16,
        llsv: AlarmSeverity::Major as i16,
    });

    inst.evaluate_alarms();
    epics_base_rs::server::recgbl::rec_gbl_reset_alarms(&mut inst.common);

    assert_eq!(inst.common.sevr, AlarmSeverity::Major);
    let afvl = inst
        .record
        .get_field("AFVL")
        .and_then(|v| v.to_f64())
        .expect("AFVL readable");
    assert!(
        (afvl - 5.0).abs() < 1e-9,
        "longin AFVL must seed to the raw HIHI alarmRange 5.0, got {afvl}"
    );
}

/// `int64in` engages the alarm-range AFTC filter (int64inRecord.c:309-310
/// seed). A HIHI integer sample seeds AFVL=5.0 and reports MAJOR.
#[test]
fn test_int64in_aftc_filter_engages_and_seeds() {
    use epics_base_rs::server::records::int64in::Int64inRecord;

    let mut rec = Int64inRecord::new(150); // above HIHI
    rec.aftc = 5.0;
    rec.afvl = 0.0;
    let mut inst = RecordInstance::new("INT64IN:AFTC".into(), rec);
    inst.common.udf = 0;
    inst.common.analog_alarm = Some(AnalogAlarmConfig {
        hihi: AlarmLimit::Double(100.0),
        high: AlarmLimit::Double(80.0),
        low: AlarmLimit::Double(-20.0),
        lolo: AlarmLimit::Double(-40.0),
        hhsv: AlarmSeverity::Major as i16,
        hsv: AlarmSeverity::Minor as i16,
        lsv: AlarmSeverity::Minor as i16,
        llsv: AlarmSeverity::Major as i16,
    });

    inst.evaluate_alarms();
    epics_base_rs::server::recgbl::rec_gbl_reset_alarms(&mut inst.common);

    assert_eq!(inst.common.sevr, AlarmSeverity::Major);
    let afvl = inst
        .record
        .get_field("AFVL")
        .and_then(|v| v.to_f64())
        .expect("AFVL readable");
    assert!(
        (afvl - 5.0).abs() < 1e-9,
        "int64in AFVL must seed to the raw HIHI alarmRange 5.0, got {afvl}"
    );
}

/// When AFTC <= 0 the filter is disabled. C `checkAlarms` initialises the
/// local `afvl = 0` and unconditionally stores `prec->afvl = afvl`
/// (aiRecord.c:356,401), so a disabled filter drives AFVL to 0. The
/// framework owner does the same, so a stale accumulator from a prior
/// AFTC>0 run cannot mis-seed a later re-enable.
#[test]
fn test_ai_aftc_disabled_resets_stale_afvl() {
    let mut rec = AiRecord::new(0.0); // normal range, no alarm
    rec.aftc = 0.0; // filter disabled
    rec.afvl = 3.0; // stale accumulator left from a prior AFTC>0 run
    let mut inst = RecordInstance::new("AI:AFTC".into(), rec);
    inst.common.udf = 0;
    inst.common.analog_alarm = Some(AnalogAlarmConfig {
        hihi: AlarmLimit::Double(100.0),
        high: AlarmLimit::Double(80.0),
        low: AlarmLimit::Double(-20.0),
        lolo: AlarmLimit::Double(-40.0),
        hhsv: AlarmSeverity::Major as i16,
        hsv: AlarmSeverity::Minor as i16,
        lsv: AlarmSeverity::Minor as i16,
        llsv: AlarmSeverity::Major as i16,
    });

    inst.evaluate_alarms();

    let afvl = inst
        .record
        .get_field("AFVL")
        .and_then(|v| v.to_f64())
        .expect("AFVL readable");
    assert_eq!(
        afvl, 0.0,
        "AFTC<=0 must reset the stale AFVL accumulator to 0"
    );
}

/// Pins the Rust port's "LALM always advances on a VAL transition"
/// behaviour. At the `R7.0.10` pin `mbbiRecord.c:344-348` is
/// `if (val == prec->lalm || recGblSetSevr(prec, COS_ALARM, prec->cosv)) return; prec->lalm = val;`
/// — `recGblSetSevr` returns TRUE when it raises the severity
/// (`recGbl.c:242-254`), so when COSV≠0 raises COS_ALARM the `||`
/// short-circuits to the early `return` and `prec->lalm = val` is
/// skipped, leaving the next transition to re-fire COS against a stale
/// LALM. EPICS PR #817 `c9817fa59` splits the test from the call at the
/// same `:344-348`, so LALM always advances; the Rust port implements
/// that POST-fix C. Deviation from the pin, documented at
/// `MbbiRecord::check_alarms`. This test pins it end-to-end:
///   (a) one transition with COSV≠NoAlarm bumps LALM to the new val;
///   (b) a subsequent transition still fires COS because LALM was
///       updated, and LALM advances again.
#[test]
fn test_mbbi_lalm_updates_when_cosv_set() {
    use epics_base_rs::server::records::mbbi::MbbiRecord;

    let mut rec = MbbiRecord::new(0);
    rec.cosv = AlarmSeverity::Major as i16; // COSV raises COS_ALARM
    rec.put_field("LALM", EpicsValue::Enum(0)).unwrap();

    let mut inst = RecordInstance::new("MBBI:LALM".into(), rec);
    inst.common.udf = 0;

    // Transition 0 → 2: COS_ALARM fires (cosv=Major), LALM must
    // advance to 2. COS/LALM logic lives in `Record::check_alarms`
    // (C `mbbiRecord.c::checkAlarms`), the hook `process_local` runs.
    inst.record.set_val(EpicsValue::Enum(2)).unwrap();
    inst.record.check_alarms(&mut inst.common);
    inst.evaluate_alarms();
    let lalm_after_first = inst
        .record
        .get_field("LALM")
        .and_then(|v| match v {
            // LALM is DBF_USHORT (mbbiRecord.dbd.pod:623).
            EpicsValue::UShort(s) => Some(s),
            _ => None,
        })
        .expect("LALM readable");
    assert_eq!(
        lalm_after_first, 2,
        "LALM must advance to new val even when COSV fires"
    );

    // Transition 2 → 0: LALM must advance to 0. Had LALM not advanced
    // to 2 in the first cycle (the C skipped-LALM path), it would still
    // read 0 here, this transition would look like "val == lalm", and
    // the COS path would return early without updating either.
    inst.record.set_val(EpicsValue::Enum(0)).unwrap();
    inst.record.check_alarms(&mut inst.common);
    inst.evaluate_alarms();
    let lalm_after_second = inst
        .record
        .get_field("LALM")
        .and_then(|v| match v {
            // LALM is DBF_USHORT (mbbiRecord.dbd.pod:623).
            EpicsValue::UShort(s) => Some(s),
            _ => None,
        })
        .expect("LALM readable");
    assert_eq!(
        lalm_after_second, 0,
        "LALM must advance again on the next transition"
    );
    // COS alarm must have re-fired during cycle 2: the accumulator
    // (`nsev`) records the highest severity hit since the last
    // reset_alarms call. With LALM correctly advanced from 2 to 0
    // between cycles, the val=2→0 step still triggers
    // `recGblSetSevr(COS_ALARM, Major)`.
    let nsev_after_second = inst.common.nsev;
    assert_eq!(
        nsev_after_second,
        AlarmSeverity::Major,
        "COS alarm must re-fire on the second transition (LALM-update bug regression)"
    );
}

/// Sibling regression for bi: same LALM-always-updates contract.
/// The Rust port handles bi and mbbi via the same `evaluate_alarms`
/// branch structure, so any regression in one implies a regression
/// in the other.
#[test]
fn test_bi_lalm_updates_when_cosv_set() {
    let mut rec = BiRecord::new(0);
    rec.cosv = AlarmSeverity::Major as i16;
    rec.put_field("LALM", EpicsValue::Enum(0)).unwrap();

    let mut inst = RecordInstance::new("BI:LALM".into(), rec);
    inst.common.udf = 0;

    // COS/LALM logic lives in `Record::check_alarms` (C
    // `biRecord.c::checkAlarms`), the hook `process_local` runs.
    inst.record.set_val(EpicsValue::Enum(1)).unwrap();
    inst.record.check_alarms(&mut inst.common);
    inst.evaluate_alarms();
    let lalm = inst
        .record
        .get_field("LALM")
        // LALM is DBF_USHORT (biRecord.dbd.pod:213).
        .and_then(|v| match v {
            EpicsValue::UShort(s) => Some(s),
            _ => None,
        })
        .expect("LALM readable");
    assert_eq!(
        lalm, 1,
        "bi LALM must advance to new val even when COSV fires"
    );
}

/// A non-UTF-8 DESC (`0xff 0x00 0x80`) put through the common-field path
/// is stored and served back byte for byte, never U+FFFD-mangled. C
/// `dbCommon` `field(DESC,DBF_STRING) size(41)` is a fixed `char[]`, so a
/// non-UTF-8 description round-trips unchanged.
#[test]
fn desc_preserves_non_utf8_bytes() {
    let rec = AiRecord::new(0.0);
    let mut inst = RecordInstance::new("DESC:NONUTF8".into(), rec);
    let raw = vec![0xffu8, 0x00, 0x80];
    inst.put_common_field(
        "DESC",
        EpicsValue::String(PvString::from_bytes(raw.clone())),
    )
    .expect("DESC put");
    match inst.get_common_field("DESC") {
        Some(EpicsValue::String(s)) => assert_eq!(
            s.as_bytes(),
            raw.as_slice(),
            "DESC must round-trip the raw bytes, not lossily decode them"
        ),
        other => panic!("expected EpicsValue::String, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// R21: an enum-valued field's DBR_STRING form. C renders it from the FIELD's
// own string source — the record's `get_enum_str` rset for a `DBF_ENUM` VAL,
// the menu's choice list for a `DBF_MENU` field — and NEVER as the decimal
// index. The port used to index the `no_str`-trimmed GR_ENUM label list and
// fall back to the number, so a `record(mbbi,"X"){}` served its VAL as "0"
// where C serves "".
//
// The cases are the boundaries of that lookup, not a story: slot defined /
// slot in range but empty / index past the slots / no table at all. Every
// expectation below was measured on the compiled C `softIoc` (see the module
// doc on `EnumStringForm`).
// ---------------------------------------------------------------------------

/// `caget -t` of the field, i.e. a DBR_STRING (0) read: the 40-byte payload up
/// to its NUL.
fn dbr_string_of(inst: &RecordInstance, field: &str) -> String {
    let snap = inst.snapshot_for_field(field).unwrap();
    let bytes = epics_base_rs::types::encode_dbr(0, &snap).unwrap();
    let end = bytes.iter().position(|b| *b == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).into_owned()
}

fn mbbi_with_states() -> epics_base_rs::server::records::mbbi::MbbiRecord {
    use epics_base_rs::server::records::mbbi::MbbiRecord;
    let mut rec = MbbiRecord::default();
    rec.zrst = "zero".into();
    rec.onst = "one".into();
    rec
}

#[test]
fn r21_enum_val_defined_slot_renders_its_state() {
    let mut rec = mbbi_with_states();
    rec.val = 1;
    let inst = RecordInstance::new("MBBI:R21".into(), rec);
    assert_eq!(dbr_string_of(&inst, "VAL"), "one");
}

/// BOUNDARY: index inside the 16 slots but with no state string. C `strncpy`s
/// the empty state (`mbbiRecord.c:246-250`) — measured `caput VAL 5` -> `[]`.
/// The trimmed label list stops at 2 here, which is exactly what used to push
/// this case onto the decimal fallback.
#[test]
fn r21_enum_val_undefined_slot_in_range_renders_empty() {
    let mut rec = mbbi_with_states();
    rec.val = 5;
    let inst = RecordInstance::new("MBBI:R21".into(), rec);
    assert_eq!(dbr_string_of(&inst, "VAL"), "");
}

/// BOUNDARY: index past the 16 slots. Measured `caput VAL 20` ->
/// `[Illegal Value]` — with a SPACE, the mbbi/mbbo spelling.
#[test]
fn r21_enum_val_past_the_slots_renders_illegal_value() {
    let mut rec = mbbi_with_states();
    rec.val = 20;
    let inst = RecordInstance::new("MBBI:R21".into(), rec);
    assert_eq!(dbr_string_of(&inst, "VAL"), "Illegal Value");
}

/// BOUNDARY: no state strings at all. `record(mbbi,"X"){}` — the oracle case.
/// C serves the empty state; the port served "0".
#[test]
fn r21_enum_val_with_no_states_renders_empty_not_zero() {
    use epics_base_rs::server::records::mbbi::MbbiRecord;
    let inst = RecordInstance::new("MBBI:R21".into(), MbbiRecord::default());
    assert_eq!(dbr_string_of(&inst, "VAL"), "");
}

/// The two-state records index slot 1 even when ONAM is empty, where their
/// `no_str` label list has been trimmed to 1 (`boRecord.c:342-352`). Measured:
/// a `bi` with only ZNAM set, `caput VAL 1` -> `[]`.
#[test]
fn r21_binary_enum_val_empty_onam_renders_empty() {
    let mut rec = BiRecord::new(0);
    rec.znam = "off".into();
    rec.val = 1;
    let inst = RecordInstance::new("BI:R21".into(), rec);
    assert_eq!(
        inst.snapshot_for_field("VAL")
            .unwrap()
            .enums
            .unwrap()
            .strings
            .len(),
        1
    );
    assert_eq!(dbr_string_of(&inst, "VAL"), "");
}

/// A `DBF_MENU` field renders its MENU's choice, not the record's VAL states —
/// the record here has both, and they must not cross.
#[test]
fn r21_menu_field_renders_its_own_choice_not_the_records_states() {
    let rec = mbbi_with_states();
    let inst = RecordInstance::new("MBBI:R21".into(), rec);
    assert_eq!(dbr_string_of(&inst, "SCAN"), "Passive");
}
