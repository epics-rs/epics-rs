//! R9-78 — aSub drives OUTA..OUTU from VALA..VALU.
//!
//! C `aSubRecord.c::process` (232-239):
//!
//! ```c
//! /* Push the output link values */
//! if (!status) {
//!     int i;
//!     for (i = 0; i < NUM_ARGS; i++)
//!         dbPutLink(&(&prec->outa)[i], (&prec->ftva)[i], (&prec->vala)[i],
//!             (&prec->neva)[i]);
//! }
//! ```
//!
//! `status` is C's cycle status: `fetch_values()`'s return, replaced by
//! `do_sub()`'s when the fetch succeeded (216-224). So the pushes happen on
//! exactly the cycles where the inputs were read AND the subroutine ran and
//! returned 0 — one gate for all 21 links, not a per-link condition.
//!
//! The port stored OUTA..OUTU and never wrote them: a subroutine's results
//! reached VALA..VALU (and their CA monitors) but no downstream record.

use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::record::{Record, SubroutineFn};
use epics_base_rs::server::records::ai::AiRecord;
use epics_base_rs::server::records::asub_record::ASubRecord;
use epics_base_rs::server::records::stringout::StringoutRecord;
use epics_base_rs::types::EpicsValue;

/// A subroutine that writes `vals` into VALA.., then returns `status`.
fn sub_writing(vals: Vec<(&'static str, f64)>, status: i64) -> Arc<SubroutineFn> {
    Arc::new(Box::new(move |rec: &mut dyn Record| {
        for (field, v) in &vals {
            rec.put_field(field, EpicsValue::Double(*v))?;
        }
        Ok(status)
    }) as SubroutineFn)
}

async fn process(db: &PvDatabase, rec: &str) {
    let mut visited = HashSet::new();
    db.process_record_with_links(rec, &mut visited, 0)
        .await
        .unwrap();
}

async fn field(db: &PvDatabase, rec: &str, f: &str) -> Option<f64> {
    let inst = db.get_record(rec).unwrap();
    let g = inst.read();
    g.record.get_field(f).and_then(|v| v.to_f64())
}

/// db with SINK_A/SINK_B/SINK_U targets and an aSub whose subroutine writes
/// VALA/VALB/VALU and returns `status`.
async fn asub_db(status: i64) -> PvDatabase {
    let db = PvDatabase::new();
    for sink in ["SINK_A", "SINK_B", "SINK_U"] {
        db.add_record(sink, Box::new(AiRecord::new(0.0)))
            .await
            .unwrap();
    }

    let mut registry: HashMap<String, Arc<SubroutineFn>> = HashMap::new();
    registry.insert(
        "writer".into(),
        sub_writing(vec![("VALA", 11.0), ("VALB", 22.0), ("VALU", 99.0)], status),
    );
    db.install_subroutine_registry(registry).await;

    // SUBL is a DB link to a record holding the subroutine name; LFLG=READ makes
    // the framework read it and bind the routine (the realistic wiring, and the
    // one `asub_lflg_read_*` in database_tests.rs uses).
    db.add_record("NAME_HOLDER", Box::new(StringoutRecord::new("writer")))
        .await
        .unwrap();

    let mut rec = ASubRecord::default();
    rec.put_field("SUBL", EpicsValue::String("NAME_HOLDER".into()))
        .unwrap();
    rec.put_field("LFLG", EpicsValue::Short(1)).unwrap(); // READ: resolve SNAM
    rec.put_field("OUTA", EpicsValue::String("SINK_A".into()))
        .unwrap();
    rec.put_field("OUTB", EpicsValue::String("SINK_B".into()))
        .unwrap();
    rec.put_field("OUTU", EpicsValue::String("SINK_U".into()))
        .unwrap();
    db.add_record("ASUB", Box::new(rec)).await.unwrap();
    db
}

/// do_sub returns 0 — every configured OUT link is pushed, first to last.
#[epics_macros_rs::epics_test]
async fn r9_78_successful_do_sub_pushes_every_output_link() {
    let db = asub_db(0).await;

    process(&db, "ASUB").await;

    assert_eq!(
        field(&db, "SINK_A", "VAL").await,
        Some(11.0),
        "OUTA takes VALA (aSubRecord.c:236-238)"
    );
    assert_eq!(
        field(&db, "SINK_B", "VAL").await,
        Some(22.0),
        "OUTB takes VALB — the push loop walks all NUM_ARGS channels"
    );
    assert_eq!(
        field(&db, "SINK_U", "VAL").await,
        Some(99.0),
        "OUTU is the last channel; the loop reaches it too"
    );
}

/// do_sub returned non-zero — C's `if (!status)` skips the whole push loop, so
/// no OUT link is written even though VALA..VALU hold fresh values.
#[epics_macros_rs::epics_test]
async fn r9_78_failed_do_sub_pushes_nothing() {
    let db = asub_db(3).await;

    process(&db, "ASUB").await;

    assert_eq!(
        field(&db, "ASUB", "VALA").await,
        Some(11.0),
        "the subroutine still wrote its outputs into VALA"
    );
    assert_eq!(
        field(&db, "SINK_A", "VAL").await,
        Some(0.0),
        "status != 0 — C pushes no output link at all"
    );
    assert_eq!(
        field(&db, "SINK_U", "VAL").await,
        Some(0.0),
        "and that holds for every channel, not just the first"
    );
}

/// A failed INPUT link aborts fetch_values, C's `status` stays non-zero, do_sub
/// never runs — and the push loop is skipped with it.
#[epics_macros_rs::epics_test]
async fn r9_78_failed_input_fetch_pushes_nothing() {
    let db = asub_db(0).await;
    {
        let inst = db.get_record("ASUB").unwrap();
        let mut g = inst.write();
        g.record
            .put_field("INPA", EpicsValue::String("NOSUCHREC".into()))
            .unwrap();
    }

    process(&db, "ASUB").await;

    assert_eq!(
        field(&db, "SINK_A", "VAL").await,
        Some(0.0),
        "fetch_values failed -> do_sub skipped -> no OUT push (aSubRecord.c:216-239)"
    );
}

/// No subroutine bound AND SNAM empty: C `do_sub` (aSubRecord.c:459-460)
/// short-circuits an empty SNAM to `return 0` BEFORE the `pfunc == NULL` ->
/// `S_db_BadSub` check — an empty SNAM is a no-op that completes with status 0,
/// not a bad-sub. So `process`'s `if (!status)` pushes every configured OUT
/// link: a subroutine-less aSub with a preset VALA and a wired OUTA drives its
/// sink. (The real `S_db_BadSub` case is a NON-empty, unregistered SNAM; that
/// non-zero-status suppression is covered by `r9_78_failed_do_sub_pushes_nothing`.)
#[epics_macros_rs::epics_test]
async fn r9_78_empty_snam_pushes_output_links() {
    let db = PvDatabase::new();
    db.add_record("SINK_A", Box::new(AiRecord::new(0.0)))
        .await
        .unwrap();

    let mut rec = ASubRecord::default();
    rec.put_field("OUTA", EpicsValue::String("SINK_A".into()))
        .unwrap();
    rec.put_field("VALA", EpicsValue::Double(7.0)).unwrap();
    db.add_record("ASUB", Box::new(rec)).await.unwrap();

    process(&db, "ASUB").await;

    assert_eq!(
        field(&db, "SINK_A", "VAL").await,
        Some(7.0),
        "empty SNAM is C do_sub's `return 0`, so status 0 pushes OUTA <- VALA"
    );
}

/// A declared `FTVB=STRING` output carries its string through OUTB — the
/// push reads the FTVx-typed cell (C `dbPutLink(&(&prec->outa)[i],
/// (&prec->ftva)[i], ...)`, aSubRecord.c:236-238), so the sink receives a
/// string, not a numeric collapse.
#[epics_macros_rs::epics_test]
async fn r9_78_string_output_pushes_through_out_link() {
    let db = PvDatabase::new();
    db.add_record("SINK_S", Box::new(StringoutRecord::new("")))
        .await
        .unwrap();

    let mut registry: HashMap<String, Arc<SubroutineFn>> = HashMap::new();
    registry.insert(
        "swriter".into(),
        Arc::new(Box::new(|rec: &mut dyn Record| {
            rec.put_field("VALB", EpicsValue::String("armed".into()))?;
            Ok(0i64)
        }) as SubroutineFn),
    );
    db.install_subroutine_registry(registry).await;
    db.add_record("NAME_HOLDER2", Box::new(StringoutRecord::new("swriter")))
        .await
        .unwrap();

    let mut rec = ASubRecord::default();
    rec.put_field("SUBL", EpicsValue::String("NAME_HOLDER2".into()))
        .unwrap();
    rec.put_field("LFLG", EpicsValue::Short(1)).unwrap(); // READ: resolve SNAM
    rec.put_field("FTVB", EpicsValue::Short(0)).unwrap(); // STRING
    rec.put_field("OUTB", EpicsValue::String("SINK_S".into()))
        .unwrap();
    db.add_record("ASUB_S", Box::new(rec)).await.unwrap();

    process(&db, "ASUB_S").await;

    let inst = db.get_record("SINK_S").unwrap();
    let g = inst.read();
    assert_eq!(
        g.record.get_field("VAL"),
        Some(EpicsValue::String("armed".into())),
        "the typed VALB string must land in the stringout sink"
    );
}
