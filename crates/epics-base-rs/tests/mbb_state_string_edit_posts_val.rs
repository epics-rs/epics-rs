//! `mbbiRecord.c:221-226` and `mbboRecord.c:286-291`, byte-identical:
//!
//! ```c
//! init_common(prec);
//! /* Note: ZRVL..FFVL are also SPC_MOD */
//! if (fieldIndex >= mbbiRecordZRST && fieldIndex <= mbbiRecordFFST
//!         && prec->val == fieldIndex - mbbiRecordZRST) {
//!     db_post_events(prec, &prec->val, DBE_VALUE | DBE_LOG);
//! }
//! ```
//!
//! A `DBF_ENUM` channel is what an operator display subscribes to as a STRING
//! (`camonitor.c:156-165` asks for `DBR_TIME_STRING`), so re-labelling the state
//! VAL sits on changes what every such client renders while VAL itself does not
//! move. C pushes the new label out by posting VAL; without that post a CSS or
//! MEDM screen keeps drawing the old label until some unrelated value change.
//!
//! Two invariants, at each of their boundaries. The post fires exactly when the
//! edited string is the one the CURRENT VAL selects — the equality in C is the
//! gate, not a blanket post on any state edit. And it fires for the STRING
//! fields only: ZRVL..FFVL carry the same `special(SPC_MOD)` and C's own comment
//! says so, but they sit outside the index range and post nothing.

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::event_queue::EventReader;
use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::server::record::Record;
use epics_base_rs::server::records::mbbi::MbbiRecord;
use epics_base_rs::server::records::mbbo::MbboRecord;
use epics_base_rs::types::{DbFieldType, EpicsValue};

/// `record(<type>,"M") { field(ZRST,"Zero") field(ONST,"One") field(VAL,"<val>") }`
async fn db_with(rtype: &str, val: u16) -> PvDatabase {
    let db = PvDatabase::new();
    let mut rec: Box<dyn Record> = match rtype {
        "mbbi" => Box::new(MbbiRecord::default()),
        "mbbo" => Box::new(MbboRecord::default()),
        other => panic!("{other}"),
    };
    rec.put_field("ZRST", EpicsValue::String("Zero".into()))
        .unwrap();
    rec.put_field("ONST", EpicsValue::String("One".into()))
        .unwrap();
    rec.put_field("VAL", EpicsValue::Enum(val)).unwrap();
    db.add_record("M", rec).await.unwrap();
    db
}

async fn watch(
    db: &PvDatabase,
    field: &str,
    sid: u32,
    dbf: DbFieldType,
    mask: EventMask,
) -> EventReader {
    let rec = db.get_record("M").unwrap();
    let mut inst = rec.write();
    inst.add_subscriber(field, sid, dbf, mask.bits())
        .expect("subscription must be accepted")
}

async fn caput(db: &PvDatabase, field: &str, value: EpicsValue) {
    db.put_record_field_from_ca("M", field, value)
        .await
        .unwrap();
}

/// Boundary: the edited string IS the one VAL selects. Both records, because
/// the two C bodies are the same body and the port must not fix one of them.
#[epics_macros_rs::epics_test]
async fn editing_the_selected_state_label_posts_val() {
    for rtype in ["mbbi", "mbbo"] {
        let db = db_with(rtype, 0).await;
        let mut val_rx = watch(&db, "VAL", 1, DbFieldType::String, EventMask::VALUE).await;

        caput(&db, "ZRST", EpicsValue::String("OFF".into())).await;

        let event = val_rx
            .try_recv()
            .unwrap_or_else(|e| panic!("{rtype}: C posts VAL here: {e:?}"));
        assert_eq!(
            event.snapshot.value,
            EpicsValue::Enum(0),
            "{rtype}: VAL itself did not move — that is why C needs the post"
        );

        // What DID move is the label the string form renders for state 0, and
        // the post above is the only thing that pushes it to a subscriber.
        let rec = db.get_record("M").unwrap();
        let label = rec.read().record.enum_state_strings().unwrap()[0].clone();
        assert_eq!(label.as_str_lossy(), "OFF", "{rtype}");
    }
}

/// The mask is C's literal `DBE_VALUE | DBE_LOG`, so a LOG-only subscriber —
/// an archiver — is woken too.
#[epics_macros_rs::epics_test]
async fn the_post_carries_the_log_bit() {
    for rtype in ["mbbi", "mbbo"] {
        let db = db_with(rtype, 0).await;
        let mut log_rx = watch(&db, "VAL", 2, DbFieldType::String, EventMask::LOG).await;

        caput(&db, "ZRST", EpicsValue::String("OFF".into())).await;

        assert!(
            log_rx.try_recv().is_ok(),
            "{rtype}: DBE_VALUE | DBE_LOG, not DBE_VALUE alone"
        );
    }
}

/// Boundary: the other side of C's equality. VAL sits on state 1, so editing
/// state 0's label changes nothing any subscriber renders.
#[epics_macros_rs::epics_test]
async fn editing_an_unselected_state_label_posts_nothing() {
    for rtype in ["mbbi", "mbbo"] {
        let db = db_with(rtype, 1).await;
        let mut val_rx = watch(&db, "VAL", 1, DbFieldType::String, EventMask::VALUE).await;

        caput(&db, "ZRST", EpicsValue::String("OFF".into())).await;

        assert!(
            val_rx.try_recv().is_err(),
            "{rtype}: prec->val != fieldIndex - ZRST, so C posts nothing"
        );
    }
}

/// Boundary: the last state slot, to pin that the range is ZRST..=FFST and not
/// just its first entry.
#[epics_macros_rs::epics_test]
async fn the_range_reaches_the_sixteenth_state() {
    for rtype in ["mbbi", "mbbo"] {
        let db = db_with(rtype, 15).await;
        let mut val_rx = watch(&db, "VAL", 1, DbFieldType::String, EventMask::VALUE).await;

        caput(&db, "FFST", EpicsValue::String("Fifteen".into())).await;

        assert!(
            val_rx.try_recv().is_ok(),
            "{rtype}: FFST is the top of C's index range"
        );
    }
}

/// Boundary: the VALUE fields. C's comment flags that ZRVL..FFVL are `SPC_MOD`
/// too; they run `init_common` and stop short of the post, because the index
/// test excludes them.
#[epics_macros_rs::epics_test]
async fn editing_a_state_value_posts_no_val() {
    for rtype in ["mbbi", "mbbo"] {
        let db = db_with(rtype, 0).await;
        let mut val_rx = watch(&db, "VAL", 1, DbFieldType::String, EventMask::VALUE).await;

        caput(&db, "ZRVL", EpicsValue::ULong(7)).await;

        assert!(
            val_rx.try_recv().is_err(),
            "{rtype}: ZRVL is SPC_MOD but outside ZRST..FFST"
        );
    }
}
