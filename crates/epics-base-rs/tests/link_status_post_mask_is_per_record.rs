//! The link-status fields carry each record's OWN `db_post_events` mask.
//!
//! Six records own link-status fields, and they do not agree:
//!
//! ```c
//! db_post_events(pcalc, &pcalc->inav, DBE_VALUE);            /* calcoutRecord.c:404, :752, :757  */
//! db_post_events(pcalc, plinkValid, DBE_VALUE);              /* sCalcoutRecord.c:287, :569, :1015, :1045 */
//! db_post_events(pcalc, plinkValid, DBE_VALUE);              /* aCalcoutRecord.c:242, :569, :1157 */
//! db_post_events(ptran, plinkValid, DBE_VALUE | DBE_LOG);    /* transformRecord.c:741, :858, :863 */
//! db_post_events(pR, &plinkGroup->dol_status, DBE_VALUE);    /* sseqRecord.c:221, :240, :910 */
//! db_post_events(pwait, pPvStat, DBE_VALUE);                 /* swaitRecord.c:527, :550, :923 */
//! ```
//!
//! The port posted `DBE_VALUE|DBE_LOG` for all six — the first four through one
//! shared helper, sseq and swait through `post_fields`, whose default is
//! `VALUE|LOG` (`processing.rs:1229-1239`) — so a `DBE_LOG`-only subscription on
//! a calcout's INAV received an event C never sends. Making the shared helper
//! post `DBE_VALUE` instead would simply move the defect onto transform, whose C
//! really does OR `DBE_LOG` in — so the mask is the record's, passed in at the
//! call.
//!
//! `DBE_LOG` alone is the discriminator: it is in exactly one of the six masks,
//! so a LOG-only subscriber is reached by transform and by nobody else. A
//! VALUE-only subscriber is the control, reached by all six.

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::server::records::acalcout::AcalcoutRecord;
use epics_base_rs::server::records::scalcout::ScalcoutRecord;
use epics_base_rs::server::records::sseq::SseqRecord;
use epics_base_rs::server::records::swait::SwaitRecord;
use epics_base_rs::server::records::transform::TransformRecord;
use epics_base_rs::types::{DbFieldType, EpicsValue};
use std::collections::HashMap;
use std::sync::Arc;

const DB: &str = r#"
record(ai, "SRC") { field(VAL, "1") }

record(calcout,  "C")  { field(CALC, "A") }
record(scalcout, "S")  { field(CALC, "A") }
record(acalcout, "A")  { field(CALC, "A") }
record(transform,"T")  { }
record(sseq,     "Q")  { }
record(swait,    "W")  { field(CALC, "A") }
"#;

type Db = Arc<PvDatabase>;

async fn build() -> Db {
    // `scalcout`, `acalcout`, `transform`, `sseq` and `swait` are synApps
    // `calc`, not Base: an application that loads them says so, the way a real
    // one loads `calcSupport.dbd`. `calcout` is Base's and needs no
    // registration — which is also what makes it the control in the case below.
    IocBuilder::new()
        .register_record_type("scalcout", || Box::new(ScalcoutRecord::default()))
        .register_record_type("acalcout", || Box::new(AcalcoutRecord::default()))
        .register_record_type("transform", || Box::new(TransformRecord::default()))
        .register_record_type("sseq", || Box::new(SseqRecord::default()))
        .register_record_type("swait", || Box::new(SwaitRecord::default()))
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0
}

fn subscribe(
    db: &Db,
    rec: &str,
    field: &str,
    sid: u32,
    mask: EventMask,
) -> epics_base_rs::server::event_queue::EventReader {
    subscribe_typed(db, rec, field, sid, mask, DbFieldType::Enum)
}

fn subscribe_typed(
    db: &Db,
    rec: &str,
    field: &str,
    sid: u32,
    mask: EventMask,
    ftype: DbFieldType,
) -> epics_base_rs::server::event_queue::EventReader {
    let inst = db.get_record(rec).unwrap();
    let mut g = inst.write();
    g.add_subscriber(field, sid, ftype, mask.bits())
        .expect("the status field accepts a subscription")
}

/// Re-point an input link, which is what makes C re-classify and post
/// (`calcoutRecord.c::special` and its siblings).
async fn repoint(db: &Db, rec: &str, field: &str) {
    // `dbPutField`, not `dbPut`: a link field refuses the latter.
    db.put_record_field_from_ca_no_notify(rec, field, EpicsValue::String("SRC".into()))
        .await
        .unwrap();
    // The classification is scheduled onto the executor, not run inline.
    epics_base_rs::runtime::task::sleep(std::time::Duration::from_millis(50)).await;
}

/// The three calcouts: `DBE_VALUE` only, so a LOG-only subscription is silent.
#[epics_macros_rs::epics_test]
async fn the_calcouts_post_link_status_without_dbe_log() {
    let db = build().await;

    for (i, (rec, status, link)) in [("C", "INAV", "INPA"), ("S", "INAV", "INPA")]
        .into_iter()
        .enumerate()
    {
        let mut log_rx = subscribe(&db, rec, status, 100 + i as u32, EventMask::LOG);
        let mut val_rx = subscribe(&db, rec, status, 200 + i as u32, EventMask::VALUE);

        repoint(&db, rec, link).await;

        assert!(
            log_rx.try_recv().is_err(),
            "{rec}.{status}: C posts a literal DBE_VALUE, so DBE_LOG must not fire"
        );
        assert!(
            val_rx.try_recv().is_ok(),
            "{rec}.{status}: the DBE_VALUE post itself must still arrive"
        );
    }
}

/// acalcout, which owns two status families (INAV.. for the scalar links,
/// IAAV.. for the array ones); both take the same literal `DBE_VALUE`.
#[epics_macros_rs::epics_test]
async fn acalcout_posts_link_status_without_dbe_log() {
    let db = build().await;
    let mut log_rx = subscribe(&db, "A", "IAAV", 1, EventMask::LOG);
    let mut val_rx = subscribe(&db, "A", "IAAV", 2, EventMask::VALUE);

    repoint(&db, "A", "INAA").await;

    assert!(
        log_rx.try_recv().is_err(),
        "aCalcoutRecord.c:242 posts a literal DBE_VALUE"
    );
    assert!(val_rx.try_recv().is_ok());
}

/// sseq and swait own link-status fields too, and reach monitors by their own
/// `refresh_link_status` rather than the shared helper — which is why the first
/// pass at this left them on `post_fields`' `VALUE|LOG` default. C's answer for
/// both is the same literal `DBE_VALUE`.
#[epics_macros_rs::epics_test]
async fn sseq_and_swait_post_link_status_without_dbe_log() {
    let db = build().await;

    let mut q_log = subscribe(&db, "Q", "DOL1V", 1, EventMask::LOG);
    let mut q_val = subscribe(&db, "Q", "DOL1V", 2, EventMask::VALUE);
    repoint(&db, "Q", "DOL1").await;
    assert!(
        q_log.try_recv().is_err(),
        "sseqRecord.c:221 posts a literal DBE_VALUE"
    );
    assert!(
        q_val.try_recv().is_ok(),
        "Q.DOL1V: the DBE_VALUE post itself must still arrive"
    );

    let mut w_log = subscribe(&db, "W", "INAV", 3, EventMask::LOG);
    let mut w_val = subscribe(&db, "W", "INAV", 4, EventMask::VALUE);
    repoint(&db, "W", "INAN").await;
    assert!(
        w_log.try_recv().is_err(),
        "swaitRecord.c:527 posts a literal DBE_VALUE"
    );
    assert!(
        w_val.try_recv().is_ok(),
        "W.INAV: the DBE_VALUE post itself must still arrive"
    );
}

/// sseq's other out-of-band post — the machine-driven `BUSY`/`WTGn`/`ABORT`/
/// `ABORTING` batch, which reaches monitors by the same `post_fields` default.
/// C posts `BUSY` at `sseqRecord.c:304` and `:1176` as a literal `DBE_VALUE`,
/// and the `asyncFinish` copies (`:481`, `:482`, `:505`) go through
/// `MonitorMask = DBE_VALUE | recGblResetAlarms(pR)` (`:471`) — an alarm bit,
/// never `DBE_LOG`.
#[epics_macros_rs::epics_test]
async fn sseq_posts_machine_status_without_dbe_log() {
    let db = build().await;
    let mut log_rx = subscribe_typed(&db, "Q", "BUSY", 5, EventMask::LOG, DbFieldType::Short);
    let mut val_rx = subscribe_typed(&db, "Q", "BUSY", 6, EventMask::VALUE, DbFieldType::Short);

    let mut visited = std::collections::HashSet::new();
    let _ = db.process_record_with_links("Q", &mut visited, 0).await;
    epics_base_rs::runtime::task::sleep(std::time::Duration::from_millis(50)).await;

    assert!(
        log_rx.try_recv().is_err(),
        "sseqRecord.c:304 posts BUSY at a literal DBE_VALUE"
    );
    assert!(
        val_rx.try_recv().is_ok(),
        "Q.BUSY: the DBE_VALUE post itself must still arrive"
    );
}

/// transform is the boundary: its C really does OR `DBE_LOG` in, so the
/// LOG-only subscription MUST be reached. A blanket `DBE_VALUE` in the shared
/// helper would have broken exactly this.
#[epics_macros_rs::epics_test]
async fn transform_posts_link_status_with_dbe_log() {
    let db = build().await;
    let mut log_rx = subscribe(&db, "T", "IAV", 1, EventMask::LOG);

    repoint(&db, "T", "INPA").await;

    assert!(
        log_rx.try_recv().is_ok(),
        "transformRecord.c:741 posts DBE_VALUE|DBE_LOG"
    );
}
