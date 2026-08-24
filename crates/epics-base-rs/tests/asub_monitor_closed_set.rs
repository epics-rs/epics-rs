//! B5 — a process cycle of an aSub may post `VAL` and, gated by `EFLG`, the
//! `VALA..VALU` / `NEVA..NEVU` pairs. Nothing else.
//!
//! C `aSubRecord.c` contains exactly five `db_post_events` calls, all inside
//! `monitor()` (`:405-451`):
//!
//! ```c
//! if (prec->val != prec->oval) { db_post_events(prec, &prec->val, monitor_mask); ... }
//! switch (prec->eflg) {
//! case aSubEFLG_NEVER:     break;
//! case aSubEFLG_ON_CHANGE: /* posts vala[i] on nev != onv || memcmp, neva[i] on nev != onv */
//! case aSubEFLG_ALWAYS:    /* posts vala[i] and neva[i] unconditionally */
//! }
//! ```
//!
//! `fetch_values` (`:250-289`) writes `a[i]` and `nea[i]` with a bare
//! `dbGetLink`, and under `LFLG=READ` writes `snam`/`onam` too — and posts
//! none of them. So a `camonitor X.A X.NEA` against a C IOC is silent
//! forever, however fast `INPA`'s source moves.
//!
//! The port modelled the `EFLG` switch as a BLACKLIST: `event_posted_fields`
//! excluded the output pairs when `EFLG == NEVER`, and everything the
//! blacklist did not name fell through to the framework's generic
//! change-detection loop. The input side is exactly what it did not name, so
//! every cycle that moved `INPA` emitted `.A` (and `.NEA` whenever the
//! delivered count moved) — a stream C cannot produce, which an archiver
//! subscribed to `.A` would record.
//!
//! The fix states the C set as a WHITELIST (`process_posted_fields`), so a
//! field is silent unless C names it. These cases are the boundaries of that
//! set: each `EFLG` arm crossed with the input side, the output side, and the
//! deadband field.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::event_queue::EventReader;
use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::server::record::{Record, SubroutineFn};
use epics_base_rs::server::records::asub_record::ASubRecord;
use epics_base_rs::server::records::waveform::WaveformRecord;
use epics_base_rs::types::{DbFieldType, EpicsValue};

/// `menu(aSubEFLG)` indices (`aSubRecord.dbd.pod`).
const EFLG_NEVER: i16 = 0;
const EFLG_ON_CHANGE: i16 = 1;
const EFLG_ALWAYS: i16 = 2;

/// `menuFtype` DOUBLE.
const FT_DOUBLE: i16 = 10;

/// Copies the input channel `A` into the output channel `VALA`, so one cycle
/// moves both sides at once.
fn copy_a_to_vala() -> Arc<SubroutineFn> {
    Arc::new(Box::new(|rec: &mut dyn Record| {
        let a = rec.get_field("A").unwrap_or(EpicsValue::Double(0.0));
        rec.put_field("VALA", a)?;
        Ok(0_i64)
    }) as SubroutineFn)
}

async fn process(db: &PvDatabase, name: &str) {
    let mut visited = HashSet::new();
    db.process_record_with_links(name, &mut visited, 0)
        .await
        .unwrap();
}

/// Drive the waveform source and settle it, so the aSub's next cycle fetches
/// the new content and the new NORD.
async fn drive(db: &PvDatabase, v: Vec<f64>) {
    db.put_record_field_from_ca("SRC", "VAL", EpicsValue::DoubleArray(v))
        .await
        .unwrap();
    process(db, "SRC").await;
}

/// `SRC` is a 3-element double waveform read by `X.INPA` into a 3-wide input
/// channel, so both `A` and `NEA` move when the source's length changes.
async fn asub_reading_a_waveform(eflg: i16) -> PvDatabase {
    let db = PvDatabase::new();
    db.add_record("SRC", Box::new(WaveformRecord::new(3, DbFieldType::Double)))
        .await
        .unwrap();

    let mut x = ASubRecord::default();
    x.put_field("INPA", EpicsValue::String("SRC".into()))
        .unwrap();
    x.put_field("FTA", EpicsValue::Short(FT_DOUBLE)).unwrap();
    x.put_field("NOA", EpicsValue::Long(3)).unwrap();
    x.put_field("FTVA", EpicsValue::Short(FT_DOUBLE)).unwrap();
    x.put_field("NOVA", EpicsValue::Long(3)).unwrap();
    x.put_field("EFLG", EpicsValue::Short(eflg)).unwrap();
    x.put_field("SNAM", EpicsValue::String("copy".into()))
        .unwrap();
    db.add_record("X", Box::new(x)).await.unwrap();

    let mut registry: HashMap<String, Arc<SubroutineFn>> = HashMap::new();
    registry.insert("copy".into(), copy_a_to_vala());
    db.install_subroutine_registry(registry.clone()).await;
    db.get_record("X").unwrap().write().subroutine = registry.get("copy").cloned();
    db
}

/// Subscribe to `field` with the full mask.
fn subscribe(db: &PvDatabase, rec: &str, field: &str, id: u32) -> EventReader {
    let inst = db.get_record(rec).unwrap();
    let mut g = inst.write();
    let full = (EventMask::VALUE | EventMask::LOG | EventMask::ALARM).bits();
    g.add_subscriber(field, id, DbFieldType::Double, full)
        .unwrap_or_else(|| panic!("a {field} subscription must be accepted"))
}

/// The finding's own trigger: `INPA`'s source moves, and `.A` / `.NEA` stay
/// silent. `EFLG` does not enter into it — the switch gates only the output
/// pairs — so all three arms are asserted.
#[epics_macros_rs::epics_test]
async fn asub_input_channels_never_post_from_a_process_cycle() {
    for eflg in [EFLG_NEVER, EFLG_ON_CHANGE, EFLG_ALWAYS] {
        let db = asub_reading_a_waveform(eflg).await;
        let mut a_rx = subscribe(&db, "X", "A", 1);
        let mut nea_rx = subscribe(&db, "X", "NEA", 2);

        // Cycle 1: A takes [1.0], NEA moves 0 -> 1.
        drive(&db, vec![1.0]).await;
        process(&db, "X").await;
        // Cycle 2: the content AND the length move.
        drive(&db, vec![7.0, 8.0]).await;
        process(&db, "X").await;

        {
            let inst = db.get_record("X").unwrap();
            let g = inst.read();
            assert_eq!(
                g.record.get_field("A"),
                Some(EpicsValue::DoubleArray(vec![7.0, 8.0])),
                "EFLG={eflg}: the cycles really did move the input channel"
            );
            assert_eq!(
                g.record.get_field("NEA"),
                Some(EpicsValue::Long(2)),
                "EFLG={eflg}: and really did move the delivered count"
            );
        }

        assert!(
            a_rx.try_recv().is_err(),
            "EFLG={eflg}: aSubRecord.c has no db_post_events for a[i] — \
             camonitor X.A is silent on a C IOC however far INPA moves"
        );
        assert!(
            nea_rx.try_recv().is_err(),
            "EFLG={eflg}: nor for nea[i] — fetch_values writes it with a bare \
             dbGetLink and posts nothing"
        );
    }
}

/// `VAL` is outside the `EFLG` switch: C posts it on `val != oval` in every
/// arm. The closed set must admit it — including under `EFLG=NEVER`, where
/// the set is otherwise empty.
#[epics_macros_rs::epics_test]
async fn asub_val_posts_in_every_eflg_arm() {
    for eflg in [EFLG_NEVER, EFLG_ON_CHANGE, EFLG_ALWAYS] {
        let db = asub_reading_a_waveform(eflg).await;
        // A subroutine whose return status moves 0 -> 3 on the second cycle.
        let bump: Arc<SubroutineFn> = {
            let seen = std::sync::atomic::AtomicI64::new(0);
            Arc::new(Box::new(move |_rec: &mut dyn Record| {
                Ok(seen.fetch_add(3, std::sync::atomic::Ordering::Relaxed))
            }) as SubroutineFn)
        };
        db.get_record("X").unwrap().write().subroutine = Some(bump);

        let mut val_rx = subscribe(&db, "X", "VAL", 3);
        process(&db, "X").await; // VAL: 0
        while val_rx.try_recv().is_ok() {}
        process(&db, "X").await; // VAL: 0 -> 3

        let e = val_rx
            .try_recv()
            .unwrap_or_else(|e| panic!("EFLG={eflg}: aSubRecord.c:415 posts VAL on val != oval, outside the EFLG switch ({e:?})"));
        assert_eq!(e.snapshot.value, EpicsValue::Long(3));
    }
}

/// The `EFLG` switch itself, on the fields it actually gates. `NEVER` posts no
/// output pair; `ON CHANGE` posts the pair that moved; `ALWAYS` posts it even
/// on a cycle where nothing moved.
#[epics_macros_rs::epics_test]
async fn asub_output_pairs_follow_the_eflg_switch() {
    // NEVER — the source moves, VALA follows it internally, no event.
    {
        let db = asub_reading_a_waveform(EFLG_NEVER).await;
        let mut vala_rx = subscribe(&db, "X", "VALA", 4);
        let mut neva_rx = subscribe(&db, "X", "NEVA", 5);
        drive(&db, vec![1.0]).await;
        process(&db, "X").await;
        drive(&db, vec![7.0, 8.0]).await;
        process(&db, "X").await;
        assert_eq!(
            db.get_record("X").unwrap().read().record.get_field("VALA"),
            Some(EpicsValue::DoubleArray(vec![7.0, 8.0])),
            "the subroutine did write VALA"
        );
        assert!(vala_rx.try_recv().is_err(), "aSubEFLG_NEVER posts nothing");
        assert!(neva_rx.try_recv().is_err(), "aSubEFLG_NEVER posts nothing");
    }

    // ON CHANGE — the pair posts on the cycle that moved it, and only then.
    {
        let db = asub_reading_a_waveform(EFLG_ON_CHANGE).await;
        let mut vala_rx = subscribe(&db, "X", "VALA", 4);
        let mut neva_rx = subscribe(&db, "X", "NEVA", 5);
        drive(&db, vec![1.0]).await;
        process(&db, "X").await;
        assert!(vala_rx.try_recv().is_ok(), "VALA moved, so C posts it");
        assert!(neva_rx.try_recv().is_ok(), "NEVA moved 0 -> 1 with it");
        // A cycle that changes nothing: C's memcmp matches and nev == onv.
        process(&db, "X").await;
        assert!(
            vala_rx.try_recv().is_err(),
            "aSubEFLG_ON_CHANGE posts vala[i] only on nev != onv || memcmp"
        );
        assert!(
            neva_rx.try_recv().is_err(),
            "and neva[i] only on nev != onv"
        );
    }

    // ALWAYS — the same idle cycle posts both.
    {
        let db = asub_reading_a_waveform(EFLG_ALWAYS).await;
        let mut vala_rx = subscribe(&db, "X", "VALA", 4);
        let mut neva_rx = subscribe(&db, "X", "NEVA", 5);
        drive(&db, vec![1.0]).await;
        process(&db, "X").await;
        while vala_rx.try_recv().is_ok() {}
        while neva_rx.try_recv().is_ok() {}
        process(&db, "X").await;
        assert!(
            vala_rx.try_recv().is_ok(),
            "aSubEFLG_ALWAYS posts vala[i] unconditionally"
        );
        assert!(
            neva_rx.try_recv().is_ok(),
            "aSubEFLG_ALWAYS posts neva[i] unconditionally"
        );
    }
}
