//! R19-24: `caput MY:VALVE Open` — a `DBR_STRING` put to a record's `DBF_ENUM`
//! `VAL` — must go through C's `put_enum_str` rset slot.
//!
//! C `dbConvert.c::putStringEnum` (lines 1149-1190):
//!
//! ```c
//! if (!prset || !prset->put_enum_str) {
//!     recGblRecSupError(status, paddr, "dbPut(putStringEnum)", "put_enum_str");
//!     return status;                      /* S_db_noRSET */
//! }
//! status = prset->put_enum_str(paddr, pfrom);
//! if (!status) return status;             /* a state name matched */
//! status = prset->get_enum_strs(paddr, &enumStrs);
//! if (!status) {
//!     status = epicsParseUInt16(pfrom, &val, dbConvertBase, &end);
//!     if (!status && val < enumStrs.no_str) { *pfield = val; return 0; }
//!     status = S_db_badChoice;
//! }
//! recGblRecordError(status, paddr->precord, pfrom);
//! return status;                          /* the put FAILS; nothing stored */
//! ```
//!
//! and `mbboRecord.c:354-371` / `boRecord.c` / `biRecord.c:290-298` /
//! `mbbiRecord.c:273-291` / `busyRecord.c`, all of which are an exact,
//! case-sensitive `strncmp` against the state strings and `S_db_badChoice`
//! otherwise.
//!
//! The port had no such slot: `field_io::coerce_write_value` handed the string
//! to the field-blind `EpicsValue::convert_to`, whose `to_f64().unwrap_or(0.0)`
//! turned every state NAME into index **0**. Measured head-to-head on the same
//! `.db` before the fix — C `caput B:MBBO Two` → `New = Two`; port → `New =
//! Zero`, and `caput` exited **0**. A valve written by name never moved and
//! nothing reported an error.
//!
//! Re-measured after the fix with C's `caput` (epics-base 7.0.10.1-DEV,
//! `bin/linux-x86_64/caput`) against the Rust `softioc-rs`:
//!
//! ```text
//! caput B:MBBO Two   -> Old : B:MBBO  Zero    New : B:MBBO  Two
//! caput -c B:BO Open -> Old : B:BO    Closed  New : B:BO    Open
//! ```
//!
//! The cases below are the CONVERTER's boundaries, one per boundary, driven
//! through the same `dbPut` entry the CA server uses.

// RTEMS-EXEC-MODEL-ALLOW(7): checked - these run and pass in the feature-ON suite.

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::record::Record;
use epics_base_rs::server::records::bo::BoRecord;
use epics_base_rs::server::records::mbbo::MbboRecord;
use epics_base_rs::server::records::mbbo_direct::MbboDirectRecord;
use epics_base_rs::types::EpicsValue;

async fn db_with(name: &str, rec: Box<dyn Record>) -> PvDatabase {
    let db = PvDatabase::new();
    db.add_record(name, rec).await.unwrap();
    db
}

fn mbbo_three_states() -> Box<dyn Record> {
    let mut rec = MbboRecord::new(0);
    rec.put_field("ZRST", EpicsValue::String("Zero".into()))
        .unwrap();
    rec.put_field("ONST", EpicsValue::String("One".into()))
        .unwrap();
    rec.put_field("TWST", EpicsValue::String("Two".into()))
        .unwrap();
    Box::new(rec)
}

async fn val_of(db: &PvDatabase, name: &str) -> EpicsValue {
    let rec = db.get_record(name).unwrap();
    let inst = rec.read();
    inst.record.get_field("VAL").unwrap()
}

async fn put(db: &PvDatabase, name: &str, value: &str) -> Result<(), String> {
    db.put_record_field_from_ca(name, "VAL", EpicsValue::String(value.into()))
        .await
        .map(|_| ())
        .map_err(|e| e.to_string())
}

// --- Boundary: the name IS a defined state --------------------------------

#[tokio::test]
async fn state_name_resolves_to_its_index() {
    let db = db_with("M", mbbo_three_states()).await;

    put(&db, "M", "Two").await.unwrap();
    assert_eq!(val_of(&db, "M").await, EpicsValue::Enum(2));

    put(&db, "M", "Zero").await.unwrap();
    assert_eq!(val_of(&db, "M").await, EpicsValue::Enum(0));
}

// --- Boundary: the name is NOT a defined state ----------------------------

#[tokio::test]
async fn unmatched_name_fails_the_put_and_stores_nothing() {
    let db = db_with("M", mbbo_three_states()).await;
    put(&db, "M", "Two").await.unwrap();

    let err = put(&db, "M", "Nonesuch").await.unwrap_err();
    assert!(err.contains("Nonesuch"), "S_db_badChoice, got: {err}");
    assert_eq!(
        val_of(&db, "M").await,
        EpicsValue::Enum(2),
        "C aborts before storing (dbAccess.c:1362 `if (status) goto done`)"
    );
}

/// C's `strncmp` is case-SENSITIVE. (The `busy` record's put arm matched
/// case-insensitively AND coerced any unmatched name to state 0.)
#[tokio::test]
async fn wrong_case_is_not_a_state_name() {
    let db = db_with("M", mbbo_three_states()).await;

    assert!(put(&db, "M", "two").await.is_err());
    assert_eq!(val_of(&db, "M").await, EpicsValue::Enum(0));
}

// --- Boundary: the numeric fallback, either side of `no_str` --------------

#[tokio::test]
async fn numeric_string_below_no_str_is_accepted() {
    let db = db_with("M", mbbo_three_states()).await;

    put(&db, "M", "2").await.unwrap();
    assert_eq!(val_of(&db, "M").await, EpicsValue::Enum(2));
}

#[tokio::test]
async fn numeric_string_at_or_above_no_str_is_badchoice() {
    let db = db_with("M", mbbo_three_states()).await;
    put(&db, "M", "1").await.unwrap();

    // Three states are defined, so `no_str` is 3 and C's `val < no_str` fails.
    assert!(put(&db, "M", "3").await.is_err());
    assert_eq!(val_of(&db, "M").await, EpicsValue::Enum(1));
}

/// The `no_str` bound is the record's OWN table, not a fixed 2 or 16: a `bo`
/// with a ZNAM and an empty ONAM advertises one state (`boRecord.c:342-352`),
/// so index 1 is out of range.
#[tokio::test]
async fn binary_no_str_trim_bounds_the_numeric_fallback() {
    let mut rec = BoRecord::new(0);
    rec.put_field("ZNAM", EpicsValue::String("Closed".into()))
        .unwrap();
    let db = db_with("B", Box::new(rec)).await;

    put(&db, "B", "Closed").await.unwrap();
    assert_eq!(val_of(&db, "B").await, EpicsValue::Enum(0));
    assert!(
        put(&db, "B", "1").await.is_err(),
        "ONAM is empty: no_str is 1, so index 1 is badChoice"
    );
}

// --- Boundary: the record type has NO put_enum_str slot -------------------

/// `mbbiDirect`/`mbboDirect` leave the slot NULL (`mbboDirectRecord.c:58`
/// `#define put_enum_str NULL`) — their `VAL` is `DBF_LONG`, so a string put
/// takes C's numeric converter and a state NAME is never resolvable.
#[tokio::test]
async fn mbbo_direct_has_no_enum_state_table() {
    let rec = MbboDirectRecord::default();
    assert!(
        rec.enum_state_strings().is_none(),
        "C leaves put_enum_str/get_enum_strs NULL on the Direct records"
    );
}
