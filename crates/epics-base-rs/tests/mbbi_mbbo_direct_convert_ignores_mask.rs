//! R3-1: the mbbiDirect / mbboDirect RECORD convert must never read MASK.
//!
//! C `mbbiDirectRecord.c:155-163`:
//!
//! ```c
//!     epicsUInt32 rval = prec->rval;
//!     if (prec->shft > 0)
//!         rval >>= prec->shft;
//!     prec->val = rval;
//! ```
//!
//! C `mbboDirectRecord.c:342-349`:
//!
//! ```c
//!     prec->rval = prec->val;
//!     if (prec->shft > 0)
//!         prec->rval <<= prec->shft;
//! ```
//!
//! Neither reads `prec->mask`. MASK is device support's field: the record seeds
//! it from NOBT unshifted (`mbbiDirectRecord.c:112-113`), and the dset positions
//! it (`prec->mask <<= prec->shft`, `devMbbiDirectSoftRaw.c:48`,
//! `devMbboDirectSoftRaw.c:31`) before applying it — on the way IN to RVAL
//! (`devMbbiDirectSoftRaw.c:55`) or on the way OUT to the wire word
//! (`devMbboDirectSoftRaw.c:40`). Under asyn, `initMbbiDirect` overwrites MASK
//! outright with the driver's already-positioned hardware mask
//! (`devAsynUInt32Digital.c:1008-1009`).
//!
//! The port applied the record's own UNSHIFTED mask inside the convert, which
//! clears exactly the bits the shift places.
//!
//! The invariant these cases bound is not a value but an independence: **the
//! record convert's output is a function of (RVAL|VAL, SHFT) alone**. So each
//! boundary is checked twice, once with a narrow MASK and once with a wide one,
//! and the two must agree. Boundaries: SHFT=0, 0<SHFT<32 (where the port broke),
//! SHFT>=32, and MASK=0 (the pre-init state, which the old code special-cased).

use epics_base_rs::server::record::Record;
use epics_base_rs::server::records::mbbi_direct::MbbiDirectRecord;
use epics_base_rs::server::records::mbbo_direct::MbboDirectRecord;
use epics_base_rs::types::EpicsValue;

/// Run mbbiDirect's RVAL -> VAL convert with the given field set.
fn mbbi_direct_val(nobt: i16, shft: u16, mask: u32, rval: u32) -> u32 {
    let mut r = MbbiDirectRecord::default();
    r.nobt = nobt;
    r.shft = shft;
    r.mask = mask;
    r.rval = rval;
    r.init_record(0).unwrap();
    r.rval = rval;
    r.process().unwrap();
    r.val
}

/// Run mbboDirect's VAL -> RVAL convert with the given field set.
fn mbbo_direct_rval(nobt: i16, shft: u16, mask: u32, val: u32) -> u32 {
    let mut r = MbboDirectRecord::default();
    r.nobt = nobt;
    r.shft = shft;
    r.mask = mask;
    r.val = val;
    r.init_record(0).unwrap();
    r.val = val;
    r.process().unwrap();
    r.rval
}

/// The reported break. NOBT=4 SHFT=4 seeds MASK=0x0F; RVAL=240 is the top
/// nibble. C shifts first and never masks, so VAL=15. The port masked with the
/// unshifted 0x0F first, which zeroed the value before the shift could reach it.
#[epics_macros_rs::epics_test]
async fn mbbi_direct_shifts_the_raw_without_masking_it() {
    assert_eq!(mbbi_direct_val(4, 4, 0x0F, 240), 15);
}

/// The output twin: VAL=15 with SHFT=4 must place the nibble at bits 4..7
/// (RVAL=240). The port's post-shift `& 0x0F` cleared every bit it had set.
#[epics_macros_rs::epics_test]
async fn mbbo_direct_shifts_the_value_without_masking_it() {
    assert_eq!(mbbo_direct_rval(4, 4, 0x0F, 15), 240);
}

/// The invariant, swept across the shift boundaries: the convert's result must
/// not move when MASK moves. `0` is the pre-init/no-NOBT state the old code
/// special-cased, `0x0F` the NOBT=4 seed, `0xFFFF_FFFF` the dset's `nobt == 0`
/// override.
#[epics_macros_rs::epics_test]
async fn the_record_convert_result_is_independent_of_mask() {
    for shft in [0u16, 1, 4, 16, 31, 32, 47] {
        for rval in [0u32, 1, 15, 240, 0x8000_0000, 0xFFFF_FFFF] {
            let base = mbbi_direct_val(4, shft, 0, rval);
            for mask in [0x0Fu32, 0xF0, 0xFFFF_FFFF] {
                assert_eq!(
                    mbbi_direct_val(4, shft, mask, rval),
                    base,
                    "mbbiDirect SHFT={shft} RVAL={rval:#x} moved with MASK={mask:#x}"
                );
            }
        }
        for val in [0u32, 1, 15, 240, 0x8000_0000, 0xFFFF_FFFF] {
            let base = mbbo_direct_rval(4, shft, 0, val);
            for mask in [0x0Fu32, 0xF0, 0xFFFF_FFFF] {
                assert_eq!(
                    mbbo_direct_rval(4, shft, mask, val),
                    base,
                    "mbboDirect SHFT={shft} VAL={val:#x} moved with MASK={mask:#x}"
                );
            }
        }
    }
}

/// SHFT=0 is C's `if (prec->shft > 0)` false arm: RVAL and VAL are the same
/// word, whatever MASK holds.
#[epics_macros_rs::epics_test]
async fn shft_zero_is_an_identity_in_both_directions() {
    for mask in [0u32, 0x0F, 0xFFFF_FFFF] {
        assert_eq!(mbbi_direct_val(4, 0, mask, 0xDEAD_BEEF), 0xDEAD_BEEF);
        assert_eq!(mbbo_direct_rval(4, 0, mask, 0xDEAD_BEEF), 0xDEAD_BEEF);
    }
}

/// End to end through the real process path, with the dset that DOES own MASK.
/// `DTYP="Raw Soft Channel"` is C `devMbbiDirectSoftRaw`: it positions the mask
/// (`mask <<= shft` -> 0xF0) and applies it to RVAL, then the record convert
/// shifts. RVAL keeps the positioned raw; VAL is the nibble. Pre-fix the record
/// re-masked with the unshifted 0x0F and published VAL=0.
#[epics_macros_rs::epics_test]
async fn raw_soft_channel_masks_positioned_and_the_record_only_shifts() {
    const DB: &str = r#"
record(longin, "SRC") {
    field(VAL, "240")
}
record(mbbiDirect, "M") {
    field(DTYP, "Raw Soft Channel")
    field(INP, "SRC")
    field(NOBT, "4")
    field(SHFT, "4")
}
"#;
    let (db, _h) = epics_base_rs::server::ioc_builder::IocBuilder::new()
        .db_string(DB, &std::collections::HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();

    let mut visited = std::collections::HashSet::new();
    db.process_record_with_links("M", &mut visited, 0)
        .await
        .unwrap();

    let inst = db.get_record("M").unwrap();
    let g = inst.read();
    assert_eq!(
        g.record.get_field("MASK").unwrap(),
        EpicsValue::ULong(0x0F),
        "the record seeds MASK from NOBT UNSHIFTED and leaves it there \
         (mbbiDirectRecord.c:112-113)"
    );
    assert_eq!(
        g.record.get_field("RVAL").unwrap(),
        EpicsValue::ULong(240),
        "the dset masks with the POSITIONED mask 0xF0, so all 240 survives"
    );
    assert_eq!(
        g.record.get_field("VAL").unwrap(),
        EpicsValue::Long(15),
        "and the record convert only shifts"
    );
    assert_eq!(
        g.record.get_field("B0").unwrap(),
        EpicsValue::UChar(1),
        "the bit fields are derived from the converted VAL"
    );
}
