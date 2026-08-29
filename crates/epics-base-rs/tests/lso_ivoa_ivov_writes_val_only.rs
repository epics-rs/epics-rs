//! `lso` IVOA = "Set output to IVOV" must write the record's OWN value, and
//! only that.
//!
//! C line numbers resolve at epics-base tag `R7.0.10`.
//!
//! `lsoRecord.c::process:131-140` is the whole arm:
//!
//! ```c
//!     case menuIvoaSet_output_to_IVOV:
//!         if (!prec->pact) {
//!             size_t size = prec->sizv - 1;
//!
//!             strncpy(prec->val, prec->ivov, size);
//!             prec->val[size] = 0;
//!             prec->len = strlen(prec->val) + 1;
//!         }
//!         status = writeValue(prec);  /* write the new value */
//! ```
//!
//! `val` and `len`; nothing else. `oval` is assigned at exactly two places in
//! the whole record — `:92` (`strcpy(prec->oval, prec->val)` in the init tail)
//! and `:251` (inside `monitor()`) — and both copy it FROM `val`. It is
//! `monitor()`'s previous-value tracker, never an output staging field:
//!
//! ```c
//!     if (prec->len != prec->olen ||
//!         memcmp(prec->oval, prec->val, prec->len)) {
//!         events |= DBE_VALUE | DBE_LOG;
//!         memcpy(prec->oval, prec->val, prec->len);
//!         prec->olen = prec->len;
//!     }
//! ```
//!
//! The port's arm wrote OVAL first and VAL second. Two consequences, one
//! measured and one latent:
//!
//! * measured — `LsoRecord::put_field` has no `"OVAL"` arm, so the OVAL write
//!   returned `FieldNotFound` on the `?` and VAL was never reached. The IVOA
//!   dispatch discards that error (`let _ = ...apply_invalid_output_value(...)`,
//!   `processing.rs:3975`), so `IVOA=Set_output_to_IVOV` was a complete no-op:
//!   VAL kept its stale value, no monitor was posted, and the OUT link received
//!   the stale value where C sends IVOV.
//! * latent — had the OVAL write landed, it would have pre-seeded the tracker
//!   with the value it is supposed to be compared against, so
//!   `len != olen || memcmp(oval, val, len)` could never fire again and the VAL
//!   monitor C posts would be suppressed for the whole life of the record.
//!
//! The port wrote OVAL because the trait's default output value is
//! `OVAL.or(VAL)` — the ao/calcout convention, where OVAL *is* the computed
//! output. lso has no such field: both of its output paths carry `val`,
//! `devLsoSoft.c:21` (`dbPutLinkLS(&prec->out, prec->val, prec->len)`) for the
//! real write and `lsoRecord.c:288` (`dbPutLinkLS(&prec->siol, prec->val,
//! prec->len)`) for the SIMM redirect. So the fix is not to teach `put_field`
//! about OVAL — that would make the wrong write succeed — but to stop OVAL
//! doubling as an output staging field: the arm writes VAL, and
//! `output_link_value` names VAL.
//!
//! One case per boundary of the C arm: the value it writes, the length it
//! derives, its `sizv - 1` truncation, its per-cycle repetition, the OUT link
//! it feeds, and the monitor `monitor()` posts because OVAL was left alone.
//!
//! The port hoists the IVOA decision into the framework
//! (`processing.rs`, the single IVOA owner shared by every output record), so
//! it runs AFTER `LsoRecord::process` returns rather than inside it as C's
//! does. The post still lands on the same cycle as the write because
//! `Record::monitor_value_changed` compares live and commits `oval`/`olen` at
//! C's `monitor()` position; the same-cycle boundary is asserted for all five
//! records that carry the arm in `ivoa_ivov_posts_on_its_own_cycle.rs`.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::event_queue::EventReader;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::types::{DbFieldType, EpicsValue};

/// An `lso` with no DOL is UDF, so `recGblSetSevr(prec, UDF_ALARM,
/// INVALID_ALARM)` (`lsoRecord.c:117-118`) fires on every cycle and the
/// `prec->nsev < INVALID_ALARM` test at `:120` always takes the IVOA switch.
async fn ioc(db_text: &str) -> Arc<PvDatabase> {
    IocBuilder::new()
        .db_string(db_text, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0
}

async fn process(db: &PvDatabase, name: &str) {
    let mut visited = HashSet::new();
    db.process_record_with_links(name, &mut visited, 0)
        .await
        .unwrap();
}

fn field(db: &PvDatabase, name: &str, f: &str) -> EpicsValue {
    let inst = db.get_record(name).unwrap();
    let g = inst.read();
    g.record
        .get_field(f)
        .unwrap_or_else(|| panic!("{name}.{f} exists"))
}

/// A long-string field as text, with the trailing NUL C's `len` counts removed.
fn text(db: &PvDatabase, name: &str, f: &str) -> String {
    match field(db, name, f) {
        EpicsValue::CharArray(b) => {
            let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
            String::from_utf8_lossy(&b[..end]).into_owned()
        }
        EpicsValue::String(s) => s.as_str_lossy().into_owned(),
        other => panic!("{name}.{f} is not a string: {other:?}"),
    }
}

/// Every value-class event the subscriber has queued, as text. A long-string
/// post carries `String` when the value fits `MAX_STRING_SIZE` and `CharArray`
/// otherwise, so both carriers count.
fn drain(rx: &mut EventReader) -> Vec<String> {
    let mut out = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        match &ev.snapshot.value {
            EpicsValue::CharArray(b) => {
                let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
                out.push(String::from_utf8_lossy(&b[..end]).into_owned());
            }
            EpicsValue::String(s) => out.push(s.as_str_lossy().into_owned()),
            other => panic!("VAL post is not a string: {other:?}"),
        }
    }
    out
}

/// `strncpy(prec->val, prec->ivov, size)` — the arm's whole point.
#[epics_macros_rs::epics_test]
async fn the_arm_writes_val_from_ivov() {
    let db =
        ioc(r#"record(lso,"L") { field(IVOA,"Set output to IVOV") field(IVOV,"fault") }"#).await;
    process(&db, "L").await;
    assert_eq!(
        text(&db, "L", "VAL"),
        "fault",
        "an INVALID cycle with IVOA=Set_output_to_IVOV copies IVOV into VAL"
    );
}

/// `prec->len = strlen(prec->val) + 1` — the NUL is counted.
#[epics_macros_rs::epics_test]
async fn the_arm_derives_len_from_the_copied_value() {
    let db =
        ioc(r#"record(lso,"L") { field(IVOA,"Set output to IVOV") field(IVOV,"fault") }"#).await;
    process(&db, "L").await;
    assert_eq!(
        field(&db, "L", "LEN"),
        EpicsValue::ULong(6),
        "LEN is strlen(\"fault\") + 1, the terminator included"
    );
}

/// `size = prec->sizv - 1; ... prec->val[size] = 0` — the copy is bounded by
/// the record's own buffer, not by IVOV's 40-byte DBF_STRING.
#[epics_macros_rs::epics_test]
async fn the_arm_truncates_at_sizv_minus_one() {
    let db = ioc(r#"record(lso,"L") {
             field(IVOA,"Set output to IVOV")
             field(IVOV,"0123456789abcdefghij")
             field(SIZV,"16")
           }"#)
    .await;
    process(&db, "L").await;
    assert_eq!(
        text(&db, "L", "VAL"),
        "0123456789abcde",
        "SIZV=16 leaves 15 bytes of text plus the terminator"
    );
    assert_eq!(
        field(&db, "L", "LEN"),
        EpicsValue::ULong(16),
        "LEN is the full buffer once the copy has filled it"
    );
}

/// The arm is inside `process()`, so a later cycle re-reads IVOV. C re-runs
/// `strncpy` every INVALID cycle; it does not latch the first one.
#[epics_macros_rs::epics_test]
async fn a_later_cycle_re_reads_ivov() {
    let db =
        ioc(r#"record(lso,"L") { field(IVOA,"Set output to IVOV") field(IVOV,"fault") }"#).await;
    process(&db, "L").await;
    {
        let inst = db.get_record("L").unwrap();
        let mut g = inst.write();
        g.record
            .put_field("IVOV", EpicsValue::String("other".into()))
            .unwrap();
    }
    process(&db, "L").await;
    assert_eq!(
        text(&db, "L", "VAL"),
        "other",
        "each INVALID cycle copies the CURRENT IVOV, C's strncpy is not latched"
    );
}

/// `devLsoSoft.c::write_string:21` — `dbPutLinkLS(&prec->out, prec->val,
/// prec->len)`. The OUT link carries VAL; there is no OVAL in C's write path.
#[epics_macros_rs::epics_test]
async fn the_out_link_carries_the_ivov_value() {
    let db = ioc(r#"record(lso,"L") {
             field(IVOA,"Set output to IVOV")
             field(IVOV,"fault")
             field(OUT,"T.VAL")
           }
           record(lsi,"T") { }"#)
    .await;
    process(&db, "L").await;
    assert_eq!(
        text(&db, "T", "VAL"),
        "fault",
        "C writes prec->val to OUT, so the linked record receives IVOV"
    );
}

/// The reason the arm must leave OVAL alone. C `monitor():244-253` compares
/// `oval` against `val`; pre-seeding `oval` with the value about to be written
/// makes that comparison equal forever, so the DBE_VALUE|DBE_LOG post C makes
/// never happens.
///
/// This case is about OVAL alone, so it runs three cycles and asks only that
/// the post happen: with OVAL pre-seeded it lands on no cycle at all, however
/// many run. Which cycle it lands on is the boundary
/// `ivoa_ivov_posts_on_its_own_cycle.rs` owns.
#[epics_macros_rs::epics_test]
async fn the_untouched_oval_lets_the_value_monitor_fire() {
    let db =
        ioc(r#"record(lso,"L") { field(IVOA,"Set output to IVOV") field(IVOV,"fault") }"#).await;
    let mut rx = {
        let inst = db.get_record("L").unwrap();
        let mut g = inst.write();
        g.add_subscriber("VAL", 1, DbFieldType::Char, EventMask::VALUE.bits())
            .unwrap()
    };
    // Drain the subscription's initial snapshot; only process-time posts count.
    drain(&mut rx);

    let mut posted = Vec::new();
    for _ in 0..3 {
        process(&db, "L").await;
        posted.extend(drain(&mut rx));
    }
    assert!(
        posted.iter().any(|v| v == "fault"),
        "C's monitor() posts DBE_VALUE|DBE_LOG for the IVOV write because oval \
         still holds the pre-arm value; got {posted:?}"
    );
}
