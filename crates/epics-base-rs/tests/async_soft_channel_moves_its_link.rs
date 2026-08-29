//! `DTYP("Async Soft Channel")` must move the link, on both sides.
//!
//! "Is this DTYP soft" had two answers in the tree that disagreed on exactly
//! this value. The attach phase said yes and gave the record no device
//! support; the processing cycle and the output path said no and deferred to a
//! device that therefore did not exist. An input record then had `convert()`
//! overwrite VAL with RVAL = 0, and an output record wrote nothing — neither
//! with any alarm or diagnostic.
//!
//! GROUND TRUTH — the built C `softIoc` (7.0.10.1-DEV,
//! `/home/stevek/work/epics-base/bin/linux-x86_64/softIoc`), same `.db` as
//! below, each record processed once by `dbpf <rec>.PROC 1`:
//!
//! ```text
//! dbgf A:AI.VAL    DBF_DOUBLE:  5
//! dbgf A:AI.UDF    DBF_UCHAR:   0 = 0x0
//! dbgf A:AI.STAT   DBF_STRING:  "NO_ALARM"
//! dbgf A:LI.VAL    DBF_LONG:    5 = 0x5
//! dbgf TGT.VAL     DBF_LONG:    7 = 0x7
//! dbgf TGT2.VAL    DBF_DOUBLE:  3.5
//! ```
//!
//! C reaches those values through `devXxxSoftCallback.c`, whose asynchrony is
//! the only thing that separates it from the plain soft dset: `write_ao` is
//! `dbPutLinkAsync(out, DBR_DOUBLE, &oval, 1)` with a synchronous `dbPutLink`
//! fallback (`devAoSoftCallback.c:41-54`), and `read_ai` returns 2 —
//! do-not-convert — on every terminal path (`devAiSoftCallback.c:167-216`).
//! This port applies the link synchronously, so it owes the same values.
//!
//! Real breakage this closes: `pva2pva/testApp/testpvalink.db:30-35`, a
//! `longout "async:trig"` with this DTYP driving a pva OUT link, shipped in
//! `epics-modules/pvxs/test/` as well.

use std::collections::HashMap;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;

/// Both sides in one database: an input with an RVAL→VAL conversion step
/// (`ai`), one without (`longin`), and the two output flavours (`longout`,
/// `ao`). `SRC` is the value the inputs must land on; `TGT`/`TGT2` are what the
/// outputs must reach.
const DB: &str = r#"
record(ai, "SRC") { field(VAL, "5") }
record(ai, "A:AI") {
    field(DTYP, "Async Soft Channel")
    field(INP,  "SRC")
}
record(longin, "A:LI") {
    field(DTYP, "Async Soft Channel")
    field(INP,  "SRC")
}
record(longout, "A:LO") {
    field(DTYP, "Async Soft Channel")
    field(OUT,  "TGT.VAL")
    field(VAL,  "7")
}
record(longin, "TGT") { }
record(ao, "A:AO") {
    field(DTYP, "Async Soft Channel")
    field(OUT,  "TGT2.VAL")
    field(VAL,  "3.5")
}
record(ai, "TGT2") { }
"#;

async fn build() -> std::sync::Arc<PvDatabase> {
    IocBuilder::new()
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0
}

async fn process(db: &PvDatabase, name: &str) {
    let mut visited = std::collections::HashSet::new();
    db.process_record_with_links(name, &mut visited, 0)
        .await
        .unwrap();
}

fn value(db: &PvDatabase, name: &str, field: &str) -> f64 {
    let rec = db
        .get_record(name)
        .unwrap_or_else(|| panic!("{name} exists"));
    let inst = rec.read();
    inst.record
        .get_field(field)
        .and_then(|v| v.to_f64())
        .unwrap_or_else(|| panic!("{name}.{field} has a numeric value"))
}

/// The input half. `A:AI` is the type that broke: `ai` runs `convert()` when
/// the dset does not claim the value, and RVAL is 0, so VAL read 0 with UDF
/// clear and no alarm. `A:LI` has no conversion step and was already right —
/// it is here so a fix that only touches the convert gate cannot pass alone.
#[epics_macros_rs::epics_test]
async fn an_async_soft_input_reads_its_inp_link() {
    let db = build().await;

    process(&db, "A:AI").await;
    process(&db, "A:LI").await;

    assert_eq!(value(&db, "A:AI", "VAL"), 5.0, "C reads 5, not RVAL 0");
    assert_eq!(value(&db, "A:LI", "VAL"), 5.0);

    let rec = db.get_record("A:AI").unwrap();
    let inst = rec.read();
    assert_eq!(inst.common.udf, 0, "C clears UDF");
    assert_eq!(inst.common.stat, 0, "C leaves STAT NO_ALARM");
}

/// The output half, where every record type was affected: the dset C runs puts
/// VAL/OVAL on the OUT link, and the port returned "a device owns this write"
/// for a record that has no device.
#[epics_macros_rs::epics_test]
async fn an_async_soft_output_writes_its_out_link() {
    let db = build().await;

    process(&db, "A:LO").await;
    process(&db, "A:AO").await;

    assert_eq!(value(&db, "TGT", "VAL"), 7.0, "the longout must reach TGT");
    assert_eq!(value(&db, "TGT2", "VAL"), 3.5, "the ao must reach TGT2");
}

/// The neighbouring flavours, so unifying the predicate cannot quietly move
/// them: `"Raw Soft Channel"` still runs the RVAL→VAL convert its dset asks for
/// (`devAiSoftRaw` returns 0), and a DTYP naming real device support still gets
/// no soft write.
#[epics_macros_rs::epics_test]
async fn the_other_flavours_keep_their_own_answers() {
    use epics_base_rs::server::device_support::{SoftDtyp, classify_soft, is_soft_dtyp};

    assert_eq!(classify_soft(""), Some(SoftDtyp::Plain));
    assert_eq!(classify_soft("Soft Channel"), Some(SoftDtyp::Plain));
    assert_eq!(classify_soft("Raw Soft Channel"), Some(SoftDtyp::Raw));
    assert_eq!(classify_soft("Async Soft Channel"), Some(SoftDtyp::Async));
    assert_eq!(classify_soft("asynInt32"), None);
    // The attach phase's question is unchanged by the split: all three
    // flavours are framework-owned and look up no registered device.
    for dtyp in ["", "Soft Channel", "Raw Soft Channel", "Async Soft Channel"] {
        assert!(is_soft_dtyp(dtyp), "{dtyp:?} needs no device registration");
    }
    assert!(!is_soft_dtyp("Soft Timestamp"));

    let db = IocBuilder::new()
        .db_string(
            r#"
record(ai, "RAWSRC") { field(VAL, "5") }
record(ai, "R:AI") {
    field(DTYP, "Raw Soft Channel")
    field(INP,  "RAWSRC")
    field(ASLO, "2")
}
"#,
            &HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;

    process(&db, "R:AI").await;
    // The raw dset puts the reading in RVAL and lets the record convert:
    // `VAL = RVAL * ASLO`. An `Async`-style skip here would leave VAL at the
    // unconverted 5.
    assert_eq!(value(&db, "R:AI", "RVAL"), 5.0);
    assert_eq!(value(&db, "R:AI", "VAL"), 10.0, "Raw still runs convert()");
}
