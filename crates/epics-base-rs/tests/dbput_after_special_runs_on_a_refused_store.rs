//! C `dbPut` runs its after-store `special()` pass even when the store failed.
//!
//! ```c
//! /* dbAccess.c:1398-1404 */
//! /* Always do special processing if needed */
//! if (special) {
//!     long status2 = dbPutSpecial(paddr, 1);
//!     if (status2)
//!         status = status2;
//! }
//! if (status) goto done;
//! ```
//!
//! Two rules in five lines: the pass runs whatever the store returned, and its
//! status REPLACES the store's. `goto done` then skips the UDF clear and the
//! field's monitor post — which is what the port's `return Err(..)` already did,
//! and all it did: the four `dbPut` bodies returned the store error before the
//! pass, so pass 0 ran and pass 1 did not. That leaves the pair unbalanced —
//! `special_before_put` has already latched OLDSIMM through `recGblSaveSimm`
//! with nothing left to consume it — and it drops the state pass 1 re-derives.
//!
//! That `dbPut` behaves this way is deliberate in C, not an oversight:
//! `dbPutFieldLink` gates the same pair on success (`dbAccess.c:1174` and
//! `:1178`, `if (!status && special)`).
//!
//! Every `dbPut` body is covered, the way `dbput_pass0_special_all_bodies.rs`
//! covers pass 0: `put_pv_body`, `put_pv_and_post_with_origin`,
//! `put_record_field_from_ca_body` and `put_pv_no_process`.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use epics_base_rs::error::{CaError, CaResult};
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::record::{FieldDesc, Record};
use epics_base_rs::types::{DbFieldType, EpicsValue};

/// A record whose store can be REFUSED on a value the field's own type accepts,
/// which is the only way to reach C's `status != 0` past `dbput_request`'s
/// coercion — the shape `aCalcoutRecord.c`'s `NUSE` arm has, moved to the store
/// so the two halves can be observed apart.
struct RefusingRecord {
    val: f64,
    pass0: Arc<AtomicU32>,
    pass1: Arc<AtomicU32>,
    /// C's `status2`: pass 1 itself failing, whose status must WIN.
    pass1_fails: bool,
}

impl Record for RefusingRecord {
    fn record_type(&self) -> &'static str {
        "test_refuse"
    }
    fn get_field(&self, name: &str) -> Option<EpicsValue> {
        match name {
            "VAL" => Some(EpicsValue::Double(self.val)),
            _ => None,
        }
    }
    fn put_field(&mut self, name: &str, value: EpicsValue) -> CaResult<()> {
        match name {
            "VAL" => match value {
                EpicsValue::Double(v) if v < 0.0 => {
                    Err(CaError::InvalidValue("negative VAL refused".into()))
                }
                EpicsValue::Double(v) => {
                    self.val = v;
                    Ok(())
                }
                other => Err(CaError::InvalidValue(format!(
                    "VAL takes a double: {other:?}"
                ))),
            },
            _ => Err(CaError::FieldNotFound(name.into())),
        }
    }
    fn declared_fields(&self) -> &'static [FieldDesc] {
        static FIELDS: &[FieldDesc] = &[FieldDesc::new("VAL", DbFieldType::Double, false)];
        FIELDS
    }
    fn special(&mut self, _field: &str, after: bool) -> CaResult<()> {
        if after {
            self.pass1.fetch_add(1, Ordering::SeqCst);
            if self.pass1_fails {
                return Err(CaError::BadField("pass 1 refused".into()));
            }
        } else {
            self.pass0.fetch_add(1, Ordering::SeqCst);
        }
        Ok(())
    }
}

struct Counts {
    pass0: Arc<AtomicU32>,
    pass1: Arc<AtomicU32>,
}

impl Counts {
    fn get(&self) -> (u32, u32) {
        (
            self.pass0.load(Ordering::SeqCst),
            self.pass1.load(Ordering::SeqCst),
        )
    }
}

async fn db_with(name: &str, pass1_fails: bool) -> (Arc<PvDatabase>, Counts) {
    let db = Arc::new(PvDatabase::new());
    let pass0 = Arc::new(AtomicU32::new(0));
    let pass1 = Arc::new(AtomicU32::new(0));
    db.add_record(
        name,
        Box::new(RefusingRecord {
            val: 7.0,
            pass0: pass0.clone(),
            pass1: pass1.clone(),
            pass1_fails,
        }),
    )
    .await
    .unwrap();
    (db, Counts { pass0, pass1 })
}

/// The four `dbPut` bodies, each driven by its own public entry, with a store
/// the record refuses.
async fn refuse_through(body: &str, db: &PvDatabase, rec: &str) -> CaError {
    let refused = EpicsValue::Double(-1.0);
    match body {
        "put_pv" => db.put_pv(&format!("{rec}.VAL"), refused).await.unwrap_err(),
        "put_pv_and_post" => db
            .put_pv_and_post(&format!("{rec}.VAL"), refused)
            .await
            .unwrap_err(),
        "put_record_field_from_ca" => db
            .put_record_field_from_ca(rec, "VAL", refused)
            .await
            .unwrap_err(),
        "put_pv_no_process" => db
            .put_pv_no_process(&format!("{rec}.VAL"), refused)
            .await
            .unwrap_err(),
        other => panic!("unknown body {other}"),
    }
}

const BODIES: [&str; 4] = [
    "put_pv",
    "put_pv_and_post",
    "put_record_field_from_ca",
    "put_pv_no_process",
];

/// Boundary: store refused, pass 1 succeeds. C runs the pass anyway
/// (`dbAccess.c:1398-1400`), so both passes ran exactly once and stay balanced.
#[epics_macros_rs::epics_test]
async fn a_refused_store_still_runs_the_after_pass_on_every_body() {
    for body in BODIES {
        let (db, counts) = db_with("R", false).await;
        let err = refuse_through(body, &db, "R").await;

        assert!(
            matches!(err, CaError::InvalidValue(_)),
            "{body}: the store's own status stands when pass 1 succeeds, got {err:?}"
        );
        assert_eq!(
            counts.get(),
            (1, 1),
            "{body}: `Always do special processing if needed` — pass 0 and pass 1 \
             must stay balanced across a refused store"
        );
    }
}

/// Boundary: store refused AND pass 1 fails. `if (status2) status = status2`
/// (`dbAccess.c:1401-1402`) — the after pass's status REPLACES the store's, so
/// the caller sees the pass-1 error, not `InvalidValue`.
#[epics_macros_rs::epics_test]
async fn the_after_pass_status_replaces_a_refused_stores() {
    for body in BODIES {
        let (db, counts) = db_with("R", true).await;
        let err = refuse_through(body, &db, "R").await;

        assert!(
            matches!(err, CaError::BadField(_)),
            "{body}: status2 wins, got {err:?}"
        );
        assert_eq!(counts.get(), (1, 1), "{body}: both passes still ran once");
    }
}

/// Boundary: the store SUCCEEDS. Wiring the pass onto the failure path must not
/// make it run twice on the path that already had it.
#[epics_macros_rs::epics_test]
async fn an_accepted_store_still_runs_the_after_pass_exactly_once() {
    for body in BODIES {
        let (db, counts) = db_with("R", false).await;
        let accepted = EpicsValue::Double(2.5);
        match body {
            "put_pv" => db.put_pv("R.VAL", accepted).await.unwrap(),
            "put_pv_and_post" => db.put_pv_and_post("R.VAL", accepted).await.unwrap(),
            "put_record_field_from_ca" => {
                db.put_record_field_from_ca("R", "VAL", accepted)
                    .await
                    .unwrap();
            }
            "put_pv_no_process" => db.put_pv_no_process("R.VAL", accepted).await.unwrap(),
            other => panic!("unknown body {other}"),
        }
        assert_eq!(counts.get(), (1, 1), "{body}: one put, one pass each");
        assert_eq!(
            db.get_record("R").unwrap().read().record.get_field("VAL"),
            Some(EpicsValue::Double(2.5)),
            "{body}: and the accepted value landed"
        );
    }
}

/// Boundary: `if (status) goto done` (`dbAccess.c:1404`) still skips everything
/// past the pass — the put fails and the field keeps the value it had.
#[epics_macros_rs::epics_test]
async fn a_refused_store_leaves_the_field_alone() {
    for body in BODIES {
        let (db, _counts) = db_with("R", false).await;
        let _ = refuse_through(body, &db, "R").await;
        assert_eq!(
            db.get_record("R").unwrap().read().record.get_field("VAL"),
            Some(EpicsValue::Double(7.0)),
            "{body}: the refused store wrote nothing"
        );
    }
}
