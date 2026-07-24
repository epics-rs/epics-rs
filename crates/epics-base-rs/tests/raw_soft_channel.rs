//! `DTYP="Raw Soft Channel"` — C's eight `devXxxSoftRaw.c` dsets.
//!
//! A raw dset never touches VAL. The four INPUT dsets land the link value in
//! RVAL (masking per their own rule) and return 0, so the record's own RVAL→VAL
//! `convert()` runs; the four OUTPUT dsets put RVAL — not VAL/OVAL — on the OUT
//! link. Only ai (`devAiSoftRaw`) and bi (`devBiSoftRaw`) had any of this.
//!
//! Boundaries, one case each:
//!   - input dset per record type: ai (unmasked LONG), mbbi and mbbiDirect
//!     (NOBT==0 mask default, SHFT shift). bi's MASK==0 / MASK!=0 pair is in
//!     `record_tests.rs`.
//!   - input ENTRY POINT: init-time constant INP (C loads it UNMASKED,
//!     `devBiSoftRaw.c:44`) vs the per-cycle link read (masked).
//!   - output dset per record type: ao/bo (raw word, unmasked) and
//!     mbbo/mbboDirect (`rval & mask`).
//!   - record type with NO SoftRaw dset in C (longin/longout): the DTYP must
//!     fall back to the VAL-direct soft path, not drop the value.

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

const DB: &str = r#"
record(longin, "SRC") { field(VAL, "37") }
record(longin, "SRC:BITS") { field(VAL, "293") }

record(ai, "RAI") {
    field(DTYP, "Raw Soft Channel")
    field(INP, "SRC")
}
record(ai, "RAI:CONST") {
    field(DTYP, "Raw Soft Channel")
    field(INP, "12")
}
record(mbbi, "RMBI") {
    field(DTYP, "Raw Soft Channel")
    field(INP, "SRC")
    field(ONVL, "37")
}
record(mbbi, "RMBI:SHFT") {
    field(DTYP, "Raw Soft Channel")
    field(INP, "SRC:BITS")
    field(NOBT, "4")
    field(SHFT, "4")
    field(MASK, "15")
}
record(mbbiDirect, "RMBID") {
    field(DTYP, "Raw Soft Channel")
    field(INP, "SRC:BITS")
}

record(longout, "DST:AO")   { field(VAL, "0") }
record(longout, "DST:BO")   { field(VAL, "0") }
record(longout, "DST:MBBO") { field(VAL, "0") }
record(longout, "DST:MBBO:SHFT") { field(VAL, "0") }
record(longout, "DST:MBBOD"){ field(VAL, "0") }
record(longout, "DST:LO")   { field(VAL, "0") }

record(ao, "RAO") {
    field(DTYP, "Raw Soft Channel")
    field(OUT, "DST:AO")
}
record(bo, "RBO") {
    field(DTYP, "Raw Soft Channel")
    field(OUT, "DST:BO")
}
record(mbbo, "RMBO") {
    field(DTYP, "Raw Soft Channel")
    field(OUT, "DST:MBBO")
    field(ONVL, "37")
}
record(mbbo, "RMBO:SHFT") {
    field(DTYP, "Raw Soft Channel")
    field(OUT, "DST:MBBO:SHFT")
    field(NOBT, "4")
    field(SHFT, "4")
    field(MASK, "15")
    field(ONVL, "499")
}
record(mbboDirect, "RMBOD") {
    field(DTYP, "Raw Soft Channel")
    field(OUT, "DST:MBBOD")
}

record(longin, "RLI") {
    field(DTYP, "Raw Soft Channel")
    field(INP, "SRC")
}
record(longout, "RLO") {
    field(DTYP, "Raw Soft Channel")
    field(OUT, "DST:LO")
}
"#;

async fn ioc() -> Arc<PvDatabase> {
    let (db, _) = IocBuilder::new()
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    db
}

async fn field(db: &PvDatabase, name: &str, f: &str) -> EpicsValue {
    db.get_record(name)
        .unwrap_or_else(|| panic!("{name} missing"))
        .read()
        .record
        .get_field(f)
        .unwrap_or_else(|| panic!("{name}.{f} missing"))
}

async fn process(db: &PvDatabase, name: &str) {
    let mut visited = HashSet::new();
    db.process_record_with_links(name, &mut visited, 0)
        .await
        .unwrap();
}

async fn put_val(db: &PvDatabase, name: &str, value: EpicsValue) {
    db.put_record_field_from_ca(name, "VAL", value)
        .await
        .unwrap();
}

// ---- input dsets: INP -> RVAL, then the record's convert() -> VAL ----

/// `devAiSoftRaw.c::read_ai` (52): `dbGetLink(pinp, DBR_LONG, &prec->rval)` — no
/// mask — then `aiRecord.c:158` runs `convert()`. With the default
/// LINR=NO CONVERSION / ASLO=1 / AOFF=0 that gives VAL == RVAL.
#[epics_macros_rs::epics_test]
async fn ai_raw_soft_channel_link_lands_in_rval_and_converts() {
    let db = ioc().await;
    process(&db, "RAI").await;

    assert_eq!(field(&db, "RAI", "RVAL").await, EpicsValue::Long(37));
    assert_eq!(field(&db, "RAI", "VAL").await, EpicsValue::Double(37.0));
}

/// `devAiSoftRaw.c::init_record` (44): `recGblInitConstantLink(&prec->inp,
/// DBF_LONG, &prec->rval)` — a CONSTANT INP seeds RVAL, NOT VAL, and the record
/// stays UDF until a process runs the convert. (The plain Soft Channel dset
/// seeds VAL and leaves RVAL 0, which is what the port used to do for both.)
///
/// softIoc 7.0.10.1-DEV oracle, `field(INP,"12")` + `DTYP="Raw Soft Channel"`:
/// at init RVAL=12 VAL=0 UDF=1 STAT=UDF; after `caput RAI:CONST.PROC 1`
/// RVAL=12 VAL=12 UDF=0.
#[epics_macros_rs::epics_test]
async fn ai_raw_soft_channel_constant_inp_seeds_rval_not_val_at_init() {
    let db = ioc().await;

    assert_eq!(field(&db, "RAI:CONST", "RVAL").await, EpicsValue::Long(12));
    assert_eq!(
        field(&db, "RAI:CONST", "VAL").await,
        EpicsValue::Double(0.0)
    );

    process(&db, "RAI:CONST").await;
    assert_eq!(
        field(&db, "RAI:CONST", "VAL").await,
        EpicsValue::Double(12.0)
    );
}

/// `devMbbiSoftRaw.c::read_mbbi` (72-73) masks RVAL with the dset's MASK, which
/// its `init_record` (60-64) defaulted to `0xffffffff` (NOBT==0) and shifted by
/// SHFT. `mbbiRecord.c` then matches RVAL against the state values: ONVL=37 with
/// RVAL=37 selects state 1.
#[epics_macros_rs::epics_test]
async fn mbbi_raw_soft_channel_nobt_zero_masks_all_bits_then_matches_state() {
    let db = ioc().await;
    process(&db, "RMBI").await;

    assert_eq!(field(&db, "RMBI", "RVAL").await, EpicsValue::ULong(37));
    assert_eq!(field(&db, "RMBI", "VAL").await, EpicsValue::Enum(1));
}

/// SHFT boundary: NOBT=4/SHFT=4 makes the dset mask `0xf << 4 == 0xf0`, so bits
/// outside the field are dropped. Source 293 == 0x125; 0x125 & 0xf0 == 0x20.
#[epics_macros_rs::epics_test]
async fn mbbi_raw_soft_channel_shft_masks_out_of_field_bits() {
    let db = ioc().await;
    process(&db, "RMBI:SHFT").await;

    assert_eq!(
        field(&db, "RMBI:SHFT", "RVAL").await,
        EpicsValue::ULong(0x20)
    );
}

/// `devMbbiDirectSoftRaw.c::read_mbbi` (57-58): same mask rule; `mbbiDirect`
/// then spreads `RVAL >> SHFT` across B0..BF. 293 == 0x125, NOBT==0 so nothing
/// is masked out, and the low bits 0/2/5/8 are set.
#[epics_macros_rs::epics_test]
async fn mbbi_direct_raw_soft_channel_masks_then_spreads_bits() {
    let db = ioc().await;
    process(&db, "RMBID").await;

    assert_eq!(field(&db, "RMBID", "RVAL").await, EpicsValue::ULong(0x125));
    assert_eq!(field(&db, "RMBID", "B0").await, EpicsValue::UChar(1));
    assert_eq!(field(&db, "RMBID", "B1").await, EpicsValue::UChar(0));
    assert_eq!(field(&db, "RMBID", "B2").await, EpicsValue::UChar(1));
    assert_eq!(field(&db, "RMBID", "B5").await, EpicsValue::UChar(1));
    assert_eq!(field(&db, "RMBID", "B8").await, EpicsValue::UChar(1));
}

// ---- output dsets: the OUT link carries RVAL, not VAL/OVAL ----

/// `devAoSoftRaw.c::write_ao` (44): `dbPutLink(&prec->out, DBR_LONG,
/// &prec->rval, 1)`. The ao's `convert()` puts VAL into RVAL (default linear
/// conversion), and RVAL is what reaches the target.
#[epics_macros_rs::epics_test]
async fn ao_raw_soft_channel_writes_rval_to_out_link() {
    let db = ioc().await;
    put_val(&db, "RAO", EpicsValue::Double(37.0)).await;

    assert_eq!(field(&db, "RAO", "RVAL").await, EpicsValue::Long(37));
    assert_eq!(field(&db, "DST:AO", "VAL").await, EpicsValue::Long(37));
}

/// `devBoSoftRaw.c::write_bo` (65): the OUT link carries RVAL. `boRecord.c`
/// converts VAL=1 -> RVAL=1.
#[epics_macros_rs::epics_test]
async fn bo_raw_soft_channel_writes_rval_to_out_link() {
    let db = ioc().await;
    put_val(&db, "RBO", EpicsValue::Enum(1)).await;

    assert_eq!(field(&db, "DST:BO", "VAL").await, EpicsValue::Long(1));
}

/// `devMbboSoftRaw.c::write_mbbo` (71-75): `data = prec->rval & prec->mask`.
/// ONVL=37 and VAL=1 give RVAL=37; NOBT==0 makes the dset mask all-ones, so the
/// target sees 37 — NOT the 1 the plain Soft Channel dset would have sent.
#[epics_macros_rs::epics_test]
async fn mbbo_raw_soft_channel_writes_masked_rval_to_out_link() {
    let db = ioc().await;
    put_val(&db, "RMBO", EpicsValue::Enum(1)).await;

    assert_eq!(field(&db, "RMBO", "RVAL").await, EpicsValue::ULong(37));
    assert_eq!(field(&db, "DST:MBBO", "VAL").await, EpicsValue::Long(37));
}

/// MASK boundary on the output side (`devMbboSoftRaw.c::write_mbbo` 71-75):
/// NOBT=4/SHFT=4 makes the dset mask `0xf << 4 == 0xf0`. ONVL=499 (0x1f3) with
/// VAL=1 gives RVAL = 0x1f3 << 4 == 0x1f30 (`mbboRecord.c::convert` 428-433,
/// which does NOT mask), so the OUT link must carry 0x1f30 & 0xf0 == 0x30.
#[epics_macros_rs::epics_test]
async fn mbbo_raw_soft_channel_dset_mask_trims_rval_before_out_link() {
    let db = ioc().await;
    put_val(&db, "RMBO:SHFT", EpicsValue::Enum(1)).await;

    assert_eq!(
        field(&db, "RMBO:SHFT", "RVAL").await,
        EpicsValue::ULong(0x1f30)
    );
    assert_eq!(
        field(&db, "DST:MBBO:SHFT", "VAL").await,
        EpicsValue::Long(0x30)
    );
}

/// `devMbboDirectSoftRaw.c::write_mbbo` (71-75) with the dset's `init_record`
/// NOBT==0 rule (`mask = 0xffffffff`): nothing is trimmed, so the whole raw word
/// reaches the target — the VAL-direct dset would have sent the same number
/// here, which is exactly why the mbbo case above is the discriminating one.
#[epics_macros_rs::epics_test]
async fn mbbo_direct_raw_soft_channel_nobt_zero_passes_whole_raw_word() {
    let db = ioc().await;
    put_val(&db, "RMBOD", EpicsValue::ULong(0x125)).await;

    assert_eq!(field(&db, "RMBOD", "RVAL").await, EpicsValue::ULong(0x125));
    assert_eq!(
        field(&db, "DST:MBBOD", "VAL").await,
        EpicsValue::Long(0x125)
    );
}

// ---- the dset table is the source of truth ----

/// C ships NO `devLonginSoftRaw` / `devLongoutSoftRaw`. `Record::raw_soft_input`
/// / `Record::raw_soft_output_value` return `None` for those record types — the
/// dset table has no SoftRaw column for them — so `DTYP="Raw Soft Channel"`
/// keeps the plain VAL-direct soft path instead of routing the value into an
/// RVAL that does not exist. (C rejects that DTYP at init; the port reads soft
/// DTYPs leniently, and leniency must not turn the write into a silent no-op.)
#[epics_macros_rs::epics_test]
async fn record_without_a_softraw_dset_falls_back_to_val_direct() {
    let db = ioc().await;
    process(&db, "RLI").await;
    put_val(&db, "RLO", EpicsValue::Long(9)).await;

    assert_eq!(field(&db, "RLI", "VAL").await, EpicsValue::Long(37));
    assert_eq!(field(&db, "DST:LO", "VAL").await, EpicsValue::Long(9));
}
