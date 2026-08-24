//! A `DBR_STRING` put into a numeric field is a PARSE that can fail, not a
//! coercion that always succeeds.
//!
//! C `dbConvert.c`'s put table gives every numeric width a `putString*` routine
//! whose body is `epicsParse*` and whose non-zero status aborts `dbPut`:
//!
//! ```c
//! /* dbConvert.c:979-996 */
//! static long putStringShort(dbAddr *paddr, const void *pfrom, ...)
//! {
//!     long status = epicsParseInt16(psrc, pdst++, dbConvertBase, &end);
//!     if (status)
//!         return status;      /* dbPut fails; the field keeps its old value */
//!     ...
//! }
//! ```
//!
//! The port routed those rows through `EpicsValue::convert_to`, which is total:
//! it cannot refuse, so it coerced. `caput REC.PREC 32768` stored 32767 and
//! `caput REC.PREC notanumber` stored 0 — both puts *accepted*, where C answers
//! `ECA_PUTFAIL` and stores nothing.
//!
//! `caput` sends `DBR_STRING` for every non-ENUM channel (`caput.c:528`), so
//! this row — not the `DBR_DOUBLE` row — is the one an actual client put takes.
//!
//! # Ground truth
//!
//! Every expectation below was MEASURED against the compiled softIoc
//! (`/home/stevek/work/epics-base/bin/linux-x86_64`), `caput -c` followed by
//! `caget`; none was computed by hand.
//!
//! ```text
//! caput T:C.PREC 32768       -> ERROR ... write request failed   PREC unchanged
//! caput T:C.PREC 32767       -> 32767
//! caput T:C.PREC notanumber  -> ERROR ... write request failed   PREC unchanged
//! caput T:C.PREC 1.7         -> 1          (strtol stops at '.')
//! caput T:C.PREC 0x10        -> 16         (dbConvertBase == 0)
//! caput T:C.PREC 5volts      -> 5          (trailing text is `units`)
//! caput T:C.VAL  1e400       -> ERROR ... write request failed   VAL unchanged
//! caput T:C.VAL  NaN         -> nan
//! caput T:C.VAL  Inf         -> inf
//! caput T:LO.DRVH -1         -> -1
//! caput T:LO.DRVH 2147483648 -> ERROR ... write request failed
//! caput T:W      hi          -> ERROR ... write request failed   W unchanged
//! caput T:W      65          -> 65 0 0 ...   NORD 1
//! caput -S T:W   hi          -> 104 105 0 ... NORD 3  (DBR_CHAR, with the NUL)
//! caput T:WU     hi          -> ERROR ... write request failed   WU unchanged
//! ```
//!
//! The cases are the invariant BOUNDARIES of the row — at-limit, just-over,
//! just-under, unparseable, empty — not a narrative.

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

const DB: &str = r#"
record(calc,     "C")  { field(PREC,"3") field(VAL,"7") }
record(longout,  "LO") { field(DRVH,"5") }
record(waveform, "W")  { field(FTVL,"CHAR") field(NELM,"16") }
"#;

async fn build() -> std::sync::Arc<PvDatabase> {
    IocBuilder::new()
        .db_string(DB, &std::collections::HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0
}

/// Drive the put the way a CA client does: the value arrives as a string.
async fn caput(db: &PvDatabase, rec: &str, field: &str, text: &str) -> Result<(), String> {
    db.put_record_field_from_ca(rec, field, EpicsValue::String(text.into()))
        .await
        .map(|_| ())
        .map_err(|e| e.to_string())
}

async fn read(db: &PvDatabase, pv: &str) -> EpicsValue {
    db.get_pv(pv).unwrap()
}

/// A refused put must leave the field EXACTLY as it was. Accepting the put and
/// storing a clamped value, and refusing the put but storing anyway, are both
/// failures — so both halves are asserted together, every time.
async fn refused(db: &PvDatabase, rec: &str, field: &str, text: &str, unchanged: EpicsValue) {
    let pv = format!("{rec}.{field}");
    assert!(
        caput(db, rec, field, text).await.is_err(),
        "{pv} = {text:?} must be REFUSED (C answers ECA_PUTFAIL)"
    );
    assert_eq!(
        read(db, &pv).await,
        unchanged,
        "{pv} must still hold its pre-put value after the refusal"
    );
}

async fn stored(db: &PvDatabase, rec: &str, field: &str, text: &str, want: EpicsValue) {
    let pv = format!("{rec}.{field}");
    caput(db, rec, field, text)
        .await
        .unwrap_or_else(|e| panic!("{pv} = {text:?} must be ACCEPTED, got {e}"));
    assert_eq!(read(db, &pv).await, want, "{pv} = {text:?}");
}

#[epics_macros_rs::epics_test]
async fn short_field_at_limit_stores_and_one_past_it_is_refused() {
    let db = build().await;
    stored(&db, "C", "PREC", "32767", EpicsValue::Short(32767)).await;
    refused(&db, "C", "PREC", "32768", EpicsValue::Short(32767)).await;
    stored(&db, "C", "PREC", "-32768", EpicsValue::Short(-32768)).await;
    refused(&db, "C", "PREC", "-32769", EpicsValue::Short(-32768)).await;
}

#[epics_macros_rs::epics_test]
async fn long_field_at_limit_stores_and_one_past_it_is_refused() {
    let db = build().await;
    stored(
        &db,
        "LO",
        "DRVH",
        "2147483647",
        EpicsValue::Long(2147483647),
    )
    .await;
    refused(
        &db,
        "LO",
        "DRVH",
        "2147483648",
        EpicsValue::Long(2147483647),
    )
    .await;
    // A negative is a value, not an overflow.
    stored(&db, "LO", "DRVH", "-1", EpicsValue::Long(-1)).await;
}

#[epics_macros_rs::epics_test]
async fn unparseable_text_is_refused_not_stored_as_zero() {
    let db = build().await;
    refused(&db, "C", "PREC", "notanumber", EpicsValue::Short(3)).await;
    refused(&db, "C", "PREC", "", EpicsValue::Short(3)).await;
    refused(&db, "C", "VAL", "notanumber", EpicsValue::Double(7.0)).await;
    refused(&db, "C", "VAL", "", EpicsValue::Double(7.0)).await;
}

#[epics_macros_rs::epics_test]
async fn double_overflow_is_refused_but_nan_and_infinity_are_values() {
    let db = build().await;
    refused(&db, "C", "VAL", "1e400", EpicsValue::Double(7.0)).await;
    refused(&db, "C", "VAL", "-1e400", EpicsValue::Double(7.0)).await;
    // Underflow is a range failure too: C returns S_stdlib_underflow.
    refused(&db, "C", "VAL", "1e-320", EpicsValue::Double(7.0)).await;

    stored(&db, "C", "VAL", "Inf", EpicsValue::Double(f64::INFINITY)).await;
    stored(
        &db,
        "C",
        "VAL",
        "-Inf",
        EpicsValue::Double(f64::NEG_INFINITY),
    )
    .await;
    caput(&db, "C", "VAL", "NaN").await.expect("NaN is a value");
    let EpicsValue::Double(v) = read(&db, "C.VAL").await else {
        panic!("VAL is a double")
    };
    assert!(v.is_nan(), "NaN must be stored, not refused");
    // 1e308 fits a double; only 1e400 does not.
    stored(&db, "C", "VAL", "1e308", EpicsValue::Double(1e308)).await;
}

/// `strtol` with `dbConvertBase == 0` stops at the first character it cannot
/// use, and the trailing text is the field's `units` — never an error.
#[epics_macros_rs::epics_test]
async fn the_numeric_prefix_is_taken_and_the_rest_is_units() {
    let db = build().await;
    stored(&db, "C", "PREC", "1.7", EpicsValue::Short(1)).await;
    stored(&db, "C", "PREC", "5volts", EpicsValue::Short(5)).await;
    stored(&db, "C", "PREC", "1e2", EpicsValue::Short(1)).await;
    stored(&db, "C", "PREC", "  42", EpicsValue::Short(42)).await;
}

/// Base 0: `0x` is hex, a leading `0` is octal. The range check then applies to
/// the parsed VALUE, so `0x8000` overflows a short even though the text is short.
#[epics_macros_rs::epics_test]
async fn integers_parse_base_zero() {
    let db = build().await;
    stored(&db, "C", "PREC", "0x10", EpicsValue::Short(16)).await;
    stored(&db, "C", "PREC", "010", EpicsValue::Short(8)).await;
    refused(&db, "C", "PREC", "0x8000", EpicsValue::Short(8)).await;
    stored(&db, "C", "VAL", "0x10", EpicsValue::Double(16.0)).await;
}

/// The array row is the SAME converter, applied per element:
/// `dbPutConvertRoutine[DBR_STRING][DBF_CHAR]` is `putStringChar`
/// (dbConvert.c:941-957), whose body is `epicsParseInt8(psrc, pdst++,
/// dbConvertBase, &end)`. So a DBR_STRING put of `"hi"` into an `FTVL=CHAR`
/// waveform is REFUSED, and `"65"` stores the ONE element 65 — not the two
/// bytes of the text. The string's bytes reach the buffer only when the client
/// asks for `DBR_CHAR` (`caput -S`), which arrives as a `CharArray` and takes
/// no conversion at all. The rule does not vary with FTVL: `FTVL=UCHAR`
/// refuses `"hi"` the same way (`putStringUchar`, :960-977).
#[epics_macros_rs::epics_test]
async fn a_string_into_a_char_array_is_parsed_like_every_other_numeric() {
    let db = build().await;
    let before = read(&db, "W.VAL").await;

    assert!(
        caput(&db, "W", "VAL", "hi").await.is_err(),
        "W.VAL = \"hi\" must be REFUSED — putStringChar's epicsParseInt8 fails"
    );
    assert_eq!(
        read(&db, "W.VAL").await,
        before,
        "a refused array put stores nothing"
    );

    caput(&db, "W", "VAL", "65").await.unwrap();
    let EpicsValue::CharArray(bytes) = read(&db, "W.VAL").await else {
        panic!("FTVL=CHAR stores a CharArray")
    };
    assert_eq!(
        bytes[0], 65,
        "one PARSED element, not the two text bytes of \"65\""
    );

    // DBR_CHAR (`caput -S`): the bytes, and no conversion row at all.
    db.put_record_field_from_ca("W", "VAL", EpicsValue::CharArray(b"hi".to_vec()))
        .await
        .expect("a DBR_CHAR put carries the bytes");
    let EpicsValue::CharArray(bytes) = read(&db, "W.VAL").await else {
        panic!("FTVL=CHAR stores a CharArray")
    };
    assert_eq!(&bytes[..2], b"hi");
}
