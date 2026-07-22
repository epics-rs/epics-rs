//! Cause A: a `DBR_STRING` put into a numeric `dbCommon` field is a PARSE that
//! can fail, not a coercion that always succeeds.
//!
//! `caput` sends these fields as `DBR_STRING` (`caput.c:528`), and the dbCommon
//! numeric fields (PROC/DISP/UDF/TPRO/RPRO/BKPT) are converted by the same
//! `dbConvert.c` `putString*` → `epicsParse*` routines the record data fields
//! use. A non-zero status refuses the put. The port routed them through
//! `coerce_common_field`'s field-blind `EpicsValue::parse`, which WRAPPED
//! (`256 as u8 == 0`) and SWALLOWED the error (`Err(_) => Ok(value)`); the fix
//! routes the numeric branch through the single owner `c_parse::put_string`,
//! keyed on the field's C-DECLARED width (the DBF_UCHAR flags parse with
//! `epicsParseUInt8`), and propagates the refusal.
//!
//! # The put-notify subtlety (measured against the compiled softIoc)
//!
//! For a REJECTED PROC conversion the two CA put paths DIFFER, and it is not a
//! guess — both were run against `/home/stevek/work/epics-base`:
//!
//! ```text
//! caput    T:AI.PROC 256   -> "write request failed"; STAT=UDF   (dbPutField: no process)
//! caput -c T:AI.PROC 256   -> "write request failed"; STAT=NO_ALARM (dbNotify: processes anyway)
//! ```
//!
//! C's `putCallback` returns `didPut = 1` even when the conversion fails
//! (`dbNotify.c:528-530`), so `processNotifyCommon` (`:243-261`) still runs
//! `dbProcess` on the put-notify (`ca_put_callback`) path — the record processes
//! and the client is still told the put failed. Plain `dbPutField`
//! (`dbAccess.c:1263-1264`) processes only when `dbPut` returned 0, so the
//! fire-and-forget path does NOT process a rejected put. The port mirrors both.

// RTEMS-EXEC-MODEL-ALLOW(7): checked - these run and pass in the feature-ON suite.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use epics_base_rs::error::{CaError, CaResult};
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::record::*;
use epics_base_rs::types::{DbFieldType, EpicsValue};

/// Minimal record whose `process()` bumps a counter, so a test can say exactly
/// how many cycles a put drove. PROC/DISP/… are `dbCommon` fields the framework
/// owns, so `put_field`/`get_field` defer them via `FieldNotFound`/`None`.
struct CountingRecord {
    val: f64,
    process_count: Arc<AtomicU32>,
}

static FIELDS: &[FieldDesc] = &[FieldDesc::new("VAL", DbFieldType::Double, false)];

impl Record for CountingRecord {
    fn record_type(&self) -> &'static str {
        "counting"
    }

    fn process(&mut self) -> CaResult<ProcessOutcome> {
        self.process_count.fetch_add(1, Ordering::Relaxed);
        Ok(ProcessOutcome::complete())
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
                EpicsValue::Double(v) => {
                    self.val = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("VAL".into())),
            },
            _ => Err(CaError::FieldNotFound(name.to_string())),
        }
    }

    fn declared_fields(&self) -> &'static [FieldDesc] {
        FIELDS
    }
}

async fn record_with(count: Arc<AtomicU32>) -> Arc<PvDatabase> {
    let db = Arc::new(PvDatabase::new());
    db.add_record(
        "REC",
        Box::new(CountingRecord {
            val: 0.0,
            process_count: count,
        }),
    )
    .await
    .unwrap();
    db
}

/// Put-notify (`ca_put_callback` / WRITE_NOTIFY): the path the oracle drives.
async fn caput_notify(db: &PvDatabase, field: &str, text: &str) -> Result<(), String> {
    db.put_record_field_from_ca("REC", field, EpicsValue::String(text.into()))
        .await
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// Fire-and-forget (`ca_put` / plain WRITE): C `dbPutField` semantics.
async fn caput_ff(db: &PvDatabase, field: &str, text: &str) -> Result<(), String> {
    db.put_record_field_from_ca_no_notify("REC", field, EpicsValue::String(text.into()))
        .await
        .map_err(|e| e.to_string())
}

async fn read_proc(db: &PvDatabase) -> EpicsValue {
    db.get_pv("REC.PROC").unwrap()
}

// --- PROC rejected: the put fails, byte not stored, process depends on path ---

/// The oracle's path. `caput -c REC.PROC 256`: the value overflows DBF_UCHAR, so
/// C refuses the put (`ECA_PUTFAIL`) but `putCallback` still returns `didPut=1`,
/// so the record STILL processes (UDF cleared → NO_ALARM). Refuse the put AND
/// force-process, matching C.
#[tokio::test]
async fn proc_overflow_via_notify_is_refused_but_still_processes() {
    let count = Arc::new(AtomicU32::new(0));
    let db = record_with(count.clone()).await;

    assert!(
        caput_notify(&db, "PROC", "256").await.is_err(),
        "caput -c REC.PROC 256 must be REFUSED (256 overflows DBF_UCHAR)"
    );
    assert_eq!(
        count.load(Ordering::Relaxed),
        1,
        "C put-notify still force-processes a PROC put whose conversion failed"
    );
    assert_eq!(
        read_proc(&db).await,
        EpicsValue::UChar(0),
        "the rejected conversion stored no byte; PROC keeps its pre-put value"
    );
}

/// Same for non-numeric text (`S_stdlib_noConversion`).
#[tokio::test]
async fn proc_non_numeric_via_notify_is_refused_but_still_processes() {
    let count = Arc::new(AtomicU32::new(0));
    let db = record_with(count.clone()).await;

    assert!(
        caput_notify(&db, "PROC", "notanumber").await.is_err(),
        "caput -c REC.PROC notanumber must be REFUSED"
    );
    assert_eq!(
        count.load(Ordering::Relaxed),
        1,
        "put-notify processes anyway"
    );
    assert_eq!(read_proc(&db).await, EpicsValue::UChar(0));
}

/// The fire-and-forget path differs: plain `dbPutField` returns before
/// `dbProcess` on a non-zero conversion status, so a rejected PROC put drives NO
/// cycle. Measured: `caput REC.PROC 256` leaves STAT=UDF.
#[tokio::test]
async fn proc_overflow_fire_and_forget_is_refused_and_does_not_process() {
    let count = Arc::new(AtomicU32::new(0));
    let db = record_with(count.clone()).await;

    assert!(
        caput_ff(&db, "PROC", "256").await.is_err(),
        "caput REC.PROC 256 must be REFUSED"
    );
    assert_eq!(
        count.load(Ordering::Relaxed),
        0,
        "plain dbPutField does NOT process a put whose conversion failed"
    );
    assert_eq!(read_proc(&db).await, EpicsValue::UChar(0));
}

/// A VALID PROC put still does BOTH on either path: stores the byte AND
/// force-processes.
#[tokio::test]
async fn proc_valid_stores_the_byte_and_processes() {
    let count = Arc::new(AtomicU32::new(0));
    let db = record_with(count.clone()).await;

    caput_notify(&db, "PROC", "1")
        .await
        .expect("caput -c REC.PROC 1 must be ACCEPTED");
    assert_eq!(
        count.load(Ordering::Relaxed),
        1,
        "a valid PROC put force-processes the record exactly once"
    );
    assert_eq!(
        read_proc(&db).await,
        EpicsValue::UChar(1),
        "the raw PROC byte is retained (C never resets it)"
    );
}

// --- the DBF_UCHAR band: negatives wrap and are ACCEPTED, 256+ is refused -----

/// `caput REC.PROC -1`: `epicsParseUInt8` accepts a negative and truncates it to
/// 255 (`strtoul("-1") == ULONG_MAX`, outside the reject band). It is NOT a naive
/// `[0, 255]` rejection.
#[tokio::test]
async fn proc_negative_wraps_to_255_and_is_accepted() {
    let count = Arc::new(AtomicU32::new(0));
    let db = record_with(count.clone()).await;

    caput_notify(&db, "PROC", "-1")
        .await
        .expect("caput REC.PROC -1 must be ACCEPTED (wraps to 255)");
    assert_eq!(read_proc(&db).await, EpicsValue::UChar(255));
    assert_eq!(
        count.load(Ordering::Relaxed),
        1,
        "a valid PROC put processes"
    );
}

/// The whole in-range band lands: 255 is the last accepted value, and 128 —
/// which a *signed* Char parse would wrongly refuse — is accepted because the
/// field is C-declared DBF_UCHAR.
#[tokio::test]
async fn proc_full_uchar_band_is_accepted() {
    let count = Arc::new(AtomicU32::new(0));
    let db = record_with(count.clone()).await;

    caput_notify(&db, "PROC", "255")
        .await
        .expect("255 is the top of the DBF_UCHAR band");
    assert_eq!(read_proc(&db).await, EpicsValue::UChar(255));

    caput_notify(&db, "PROC", "128")
        .await
        .expect("128 is a valid DBF_UCHAR value, not a signed overflow");
    assert_eq!(read_proc(&db).await, EpicsValue::UChar(128));
}

// --- another dbCommon flag on the same gate: DISP -----------------------------

/// The gate is shared: `caput REC.DISP notanumber` / `256` are refused too — DISP
/// takes the same DBF_UCHAR conversion as PROC. (DISP is not pp(TRUE), so it
/// drives no process; the assertion is the refusal and that a valid put lands.)
#[tokio::test]
async fn disp_non_numeric_and_overflow_are_refused() {
    let count = Arc::new(AtomicU32::new(0));
    let db = record_with(count.clone()).await;

    assert!(
        caput_notify(&db, "DISP", "notanumber").await.is_err(),
        "caput REC.DISP notanumber must be REFUSED"
    );
    assert!(
        caput_notify(&db, "DISP", "256").await.is_err(),
        "caput REC.DISP 256 overflows DBF_UCHAR and must be REFUSED"
    );
    // A valid DISP put is accepted (stored via the same coerced path).
    caput_notify(&db, "DISP", "0")
        .await
        .expect("caput REC.DISP 0 is a valid DBF_UCHAR put");
    assert_eq!(count.load(Ordering::Relaxed), 0, "DISP is not pp(TRUE)");
}
