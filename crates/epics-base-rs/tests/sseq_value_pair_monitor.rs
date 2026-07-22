//! R17-4: every `DOn`/`STRn` event is one the WRITER made, with that C call
//! site's mask and that call site's own change test.
//!
//! `DOn` and `STRn` are two views of one value, and C posts them from four
//! places — never from a generic change loop:
//!
//! ```c
//! /* special(), DOn put (sseqRecord.c:1112-1116) — dbPut already posted DOn */
//! cvtDoubleToString(plinkGroup->dov, str, pR->prec);
//! if (strcmp(str, plinkGroup->s)) { strcpy(plinkGroup->s, str);
//!     db_post_events(pR, &plinkGroup->s, DBE_VALUE); }
//! /* special(), STRn put (:1136-1140) */
//! d = atof(plinkGroup->s);
//! if (d != plinkGroup->dov) { plinkGroup->dov = d;
//!     db_post_events(pR, &plinkGroup->dov, DBE_VALUE); }
//! /* processCallback, numeric DOL arm (:672-683) */
//! if (d != plinkGroup->dov) db_post_events(pR, &plinkGroup->dov, DBE_VALUE|DBE_LOG);
//! cvtDoubleToString(plinkGroup->dov, str, pR->prec);
//! if (strcmp(str, plinkGroup->s)) db_post_events(pR, &plinkGroup->s, DBE_VALUE);
//! ```
//!
//! Two facts fall out, and the port had neither: the DERIVED view carries a
//! BARE `DBE_VALUE` (no LOG, no alarm bit), and it posts ONLY when its own
//! comparison moved. The port posted the partner from a static field-name list
//! with `DBE_VALUE|DBE_LOG`, unconditionally — an archiver on `STR1` logged a
//! sample on every `caput DO1` that changed nothing.

// RTEMS-EXEC-MODEL-ALLOW(4): checked - these run and pass in the feature-ON suite.

use std::collections::HashSet;
use std::time::Duration;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::server::record::Record;
use epics_base_rs::server::records::ao::AoRecord;
use epics_base_rs::server::records::sseq::SseqRecord;
use epics_base_rs::types::{DbFieldType, EpicsValue};

fn full() -> u16 {
    (EventMask::VALUE | EventMask::LOG | EventMask::ALARM).bits()
}

/// An sseq with PREC=2 and one step whose LNK writes an ao — no DOL, so only
/// the put paths run.
async fn sseq_with_prec2() -> PvDatabase {
    let db = PvDatabase::new();
    db.add_record("VP_DST", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();

    let mut sseq = SseqRecord::new();
    sseq.put_field("PREC", EpicsValue::Short(2)).unwrap();
    sseq.put_field("SELM", EpicsValue::Short(0)).unwrap();
    sseq.put_field("LNK1", EpicsValue::String("VP_DST".into()))
        .unwrap();
    db.add_record("VP", Box::new(sseq)).await.unwrap();
    db
}

/// `caput VP.DO1 3.7` (PREC=2): the put path posts DO1 (C `dbPut`), and
/// `special()` posts the re-rendered STR1="3.70" with a bare `DBE_VALUE`.
#[tokio::test]
async fn r17_4_a_do_put_posts_the_derived_string_with_a_bare_value_mask() {
    let db = sseq_with_prec2().await;
    let inst = db.get_record("VP").unwrap();

    let (mut do_rx, mut str_rx) = {
        let mut g = inst.write();
        let d = g
            .add_subscriber("DO1", 1, DbFieldType::Double, full())
            .unwrap();
        let s = g
            .add_subscriber("STR1", 2, DbFieldType::String, full())
            .unwrap();
        (d, s)
    };

    db.put_record_field_from_ca("VP", "DO1", EpicsValue::Double(3.7))
        .await
        .unwrap();

    let e = do_rx.try_recv().expect("dbPut posts the field written");
    assert_eq!(e.snapshot.value, EpicsValue::Double(3.7));
    assert_eq!(
        e.mask,
        EventMask::VALUE | EventMask::LOG,
        "the WRITTEN view is posted by the put path, DBE_VALUE|DBE_LOG"
    );

    let e = str_rx
        .try_recv()
        .expect("special() re-rendered STR1 from the new DO1, so it posts");
    assert_eq!(e.snapshot.value, EpicsValue::String("3.70".into()));
    assert_eq!(
        e.mask,
        EventMask::VALUE,
        "sseqRecord.c:1115 posts the DERIVED view with a bare DBE_VALUE — \
         no DBE_LOG, so an archiver-only client is NOT notified"
    );

    assert!(do_rx.try_recv().is_err(), "DO1 posted exactly once");
    assert!(str_rx.try_recv().is_err(), "STR1 posted exactly once");
}

/// The change test C makes and a static field list cannot: re-putting the SAME
/// DO1 leaves `strcmp(str, plinkGroup->s) == 0`, so STR1 posts NOTHING.
#[tokio::test]
async fn r17_4_a_do_put_that_moves_no_string_posts_no_string_event() {
    let db = sseq_with_prec2().await;
    let inst = db.get_record("VP").unwrap();

    let mut str_rx = inst
        .write()
        .add_subscriber("STR1", 1, DbFieldType::String, full())
        .unwrap();

    db.put_record_field_from_ca("VP", "DO1", EpicsValue::Double(3.7))
        .await
        .unwrap();
    assert!(
        str_rx.try_recv().is_ok(),
        "the first put moved STR1 to \"3.70\""
    );

    // Same value again — and, at PREC=2, a DIFFERENT value that renders to the
    // same string. C compares the rendered STRING, not the double.
    db.put_record_field_from_ca("VP", "DO1", EpicsValue::Double(3.7))
        .await
        .unwrap();
    db.put_record_field_from_ca("VP", "DO1", EpicsValue::Double(3.7001))
        .await
        .unwrap();
    assert!(
        str_rx.try_recv().is_err(),
        "STR1 still renders \"3.70\": C's strcmp guard posts nothing"
    );
}

/// The mirror site: a `STRn` put derives `DOn = atof(s)` and posts it with a
/// bare `DBE_VALUE`, only when it moved (sseqRecord.c:1136-1140).
#[tokio::test]
async fn r17_4_a_str_put_posts_the_derived_double_with_a_bare_value_mask() {
    let db = sseq_with_prec2().await;
    let inst = db.get_record("VP").unwrap();

    let mut do_rx = inst
        .write()
        .add_subscriber("DO1", 1, DbFieldType::Double, full())
        .unwrap();

    db.put_record_field_from_ca("VP", "STR1", EpicsValue::String("5".into()))
        .await
        .unwrap();

    let e = do_rx.try_recv().expect("atof(\"5\") moved DO1 0 -> 5");
    assert_eq!(e.snapshot.value, EpicsValue::Double(5.0));
    assert_eq!(
        e.mask,
        EventMask::VALUE,
        "sseqRecord.c:1139 posts the derived dov with a bare DBE_VALUE"
    );

    // "5.0" is a different STRING but the same atof — C posts no dov event.
    db.put_record_field_from_ca("VP", "STR1", EpicsValue::String("5.0".into()))
        .await
        .unwrap();
    assert!(
        do_rx.try_recv().is_err(),
        "atof(\"5.0\") == dov: `if (d != plinkGroup->dov)` fails, so no post"
    );
}

/// The process path (`processCallback`, numeric DOL arm): NO put posts the
/// field, so the record owes BOTH events — the view READ carries
/// `DBE_VALUE|DBE_LOG`, the view DERIVED from it a bare `DBE_VALUE`.
#[tokio::test]
async fn r17_4_a_dol_read_posts_the_read_view_with_log_and_the_derived_view_without() {
    let db = PvDatabase::new();
    db.add_record("VPP_SRC", Box::new(AoRecord::new(7.25)))
        .await
        .unwrap();
    db.add_record("VPP_DST", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();

    let mut sseq = SseqRecord::new();
    sseq.put_field("PREC", EpicsValue::Short(2)).unwrap();
    sseq.put_field("SELM", EpicsValue::Short(0)).unwrap();
    sseq.put_field("DOL1", EpicsValue::String("VPP_SRC.VAL".into()))
        .unwrap();
    sseq.put_field("LNK1", EpicsValue::String("VPP_DST".into()))
        .unwrap();
    db.add_record("VPP", Box::new(sseq)).await.unwrap();

    let inst = db.get_record("VPP").unwrap();
    let (mut do_rx, mut str_rx) = {
        let mut g = inst.write();
        let d = g
            .add_subscriber("DO1", 1, DbFieldType::Double, full())
            .unwrap();
        let s = g
            .add_subscriber("STR1", 2, DbFieldType::String, full())
            .unwrap();
        (d, s)
    };

    let mut visited = HashSet::new();
    db.process_record_with_links("VPP", &mut visited, 0)
        .await
        .unwrap();
    for _ in 0..400 {
        if let Some(EpicsValue::Double(v)) = db
            .get_record("VPP_DST")
            .unwrap()
            .read()
            .record
            .get_field("VAL")
            && v == 7.25
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    let e = do_rx.try_recv().expect("the DOL read moved DO1 0 -> 7.25");
    assert_eq!(e.snapshot.value, EpicsValue::Double(7.25));
    assert_eq!(
        e.mask,
        EventMask::VALUE | EventMask::LOG,
        "sseqRecord.c:676 posts the view the link was READ into with DBE_VALUE|DBE_LOG"
    );

    let e = str_rx.try_recv().expect("STR1 was re-rendered to \"7.25\"");
    assert_eq!(e.snapshot.value, EpicsValue::String("7.25".into()));
    assert_eq!(
        e.mask,
        EventMask::VALUE,
        "sseqRecord.c:679 posts the DERIVED string with a bare DBE_VALUE"
    );
}
