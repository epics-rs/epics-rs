//! A string put into an ARRAY destination is C's `putString*` row, and the
//! count does not pick a different rule.
//!
//! `dbPut` takes that arm whenever `nRequest > 1` **or** the field is
//! `special(SPC_DBADDR)` (`dbAccess.c:1350`), so a one-element string put into
//! `compress.VAL` is the array row, not the scalar one:
//!
//! ```c
//! if (nRequest>1 || paddr->pfldDes->special == SPC_DBADDR) {
//!     ...
//!     status = dbPutConvertRoutine[dbrType][field_type](paddr, pbuffer,
//!         nRequest, no_elements, offset);
//! ```
//!
//! and every element goes through `epicsParse*` with a non-zero status
//! refusing the whole put (`putStringDouble`, `dbConvert.c:1130`).
//!
//! The port used to answer that one C routine in two places with two different
//! failure semantics. A scalar `String` was handed to the record untouched, on
//! the contract that array records run the row themselves — `waveform` did,
//! `compress` and `histogram` answered `TypeMismatch` to a put the compiled
//! softIoc accepts. A `StringArray` never reached the coercion owner at all and
//! fell to the total `EpicsValue::convert_to`, whose `as_f64_array` mapped
//! unparseable text to `0.0`, so `caput -a CMP 2 abc def` stored two zeros and
//! advanced the ring where C refuses the put and touches nothing.
//!
//! Measured against `softIoc` R7.0.10 on a `NSAM=3` FIFO compress, a `NELM=3`
//! histogram and a `NELM=3` `FTVL=DOUBLE` waveform.

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;
use std::collections::HashMap;
use std::sync::Arc;

const DB: &str = r#"
record(compress, "CMP") { field(NSAM, "3") field(ALG, "Circular Buffer") field(BALG, "FIFO Buffer") }
record(histogram,"HIS") { field(NELM, "3") }
record(waveform, "WF")  { field(NELM, "3") field(FTVL, "DOUBLE") }
record(printf,   "PRF") { field(SIZV, "40") field(FMT, "x") }
"#;

type Db = Arc<PvDatabase>;

async fn build() -> Db {
    IocBuilder::new()
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0
}

/// `caput REC` with no `-a`: one DBR_STRING element (`caput.c:528`).
async fn caput(db: &Db, rec: &str, text: &str) -> epics_base_rs::error::CaResult<()> {
    db.put_record_field_from_ca_no_notify(rec, "VAL", EpicsValue::String(text.into()))
        .await
        .map(|_| ())
}

/// `caput -a REC n ...`: `n` DBR_STRING elements.
async fn caput_array(db: &Db, rec: &str, texts: &[&str]) -> epics_base_rs::error::CaResult<()> {
    let arr = texts.iter().map(|t| (*t).into()).collect();
    db.put_record_field_from_ca_no_notify(rec, "VAL", EpicsValue::StringArray(arr))
        .await
        .map(|_| ())
}

/// What a CA client sees: the record's own `cvt_dbaddr`/`get_array_info` shape,
/// not the raw backing `Vec`.
fn val(db: &Db, rec: &str) -> Vec<f64> {
    match db.get_pv(&format!("{rec}.VAL")).expect("VAL") {
        EpicsValue::DoubleArray(a) => a,
        EpicsValue::ULongArray(a) => a.into_iter().map(|v| v as f64).collect(),
        EpicsValue::Double(v) => vec![v],
        EpicsValue::ULong(v) => vec![v as f64],
        other => panic!("{rec}.VAL is {other:?}"),
    }
}

fn field_f64(db: &Db, rec: &str, field: &str) -> f64 {
    db.get_record(rec)
        .unwrap()
        .read()
        .record
        .get_field(field)
        .and_then(|v| v.to_f64())
        .unwrap_or_else(|| panic!("{rec}.{field}"))
}

/// The headline: `caput CMP 7` is accepted, as `softIoc` accepts it. Before the
/// row had one owner this was `TypeMismatch`.
#[epics_macros_rs::epics_test]
async fn a_one_element_string_put_into_a_dbaddr_buffer_is_the_array_row() {
    let db = build().await;
    caput(&db, "CMP", "7")
        .await
        .expect("dbAccess.c:1350 sends a SPC_DBADDR field to the array row at any count");
    assert_eq!(val(&db, "CMP"), [7.0]);
    assert_eq!(
        field_f64(&db, "CMP", "NUSE"),
        1.0,
        "put_array_info saw one element"
    );
}

/// The same row on the second record that used to refuse it.
#[epics_macros_rs::epics_test]
async fn a_one_element_string_put_into_a_histogram_bin_array_is_accepted() {
    let db = build().await;
    caput(&db, "HIS", "7").await.expect("putStringUlong");
    assert_eq!(val(&db, "HIS"), [7.0, 0.0, 0.0]);
}

/// `histogramRecord.c:310-318` answers `*no_elements = prec->nelm` and
/// `put_array_info` is `NULL` (`:56`), so a short request writes the head and
/// leaves the tail — the bin array never narrows.
#[epics_macros_rs::epics_test]
async fn a_short_put_keeps_the_histogram_bin_array_nelm_wide() {
    let db = build().await;
    caput_array(&db, "HIS", &["4", "5"])
        .await
        .expect("two bins");
    assert_eq!(val(&db, "HIS"), [4.0, 5.0, 0.0], "NELM stays 3");
}

/// The formerly-bypassing path: a `StringArray` never reached the coercion
/// owner, so an unparseable element became `0.0` instead of refusing the put.
#[epics_macros_rs::epics_test]
async fn an_unparseable_element_refuses_the_whole_array_put() {
    let db = build().await;
    caput_array(&db, "CMP", &["4", "5"]).await.expect("numeric");
    assert_eq!(val(&db, "CMP"), [4.0, 5.0]);

    caput_array(&db, "CMP", &["abc", "def"])
        .await
        .expect_err("dbConvert.c:1130 returns non-zero and dbAccess.c:1362 gives up");
    assert_eq!(
        val(&db, "CMP"),
        [4.0, 5.0],
        "a refused put writes nothing at all"
    );
    assert_eq!(field_f64(&db, "CMP", "NUSE"), 2.0, "and moves no cursor");
}

/// One unparseable element among good ones refuses the whole request, not just
/// its own slot.
#[epics_macros_rs::epics_test]
async fn one_bad_element_refuses_the_request_its_neighbours_included() {
    let db = build().await;
    caput_array(&db, "WF", &["1", "hi", "3"])
        .await
        .expect_err("epicsParseFloat64 refuses `hi`");
    assert!(
        val(&db, "WF").is_empty(),
        "NORD stays 0 — not one element of it landed"
    );
}

/// The scalar-string refusal `waveform` already had, kept by the move: the row
/// is the same one whether the record or the coercion owner runs it.
#[epics_macros_rs::epics_test]
async fn an_unparseable_one_element_string_is_refused_too() {
    let db = build().await;
    caput(&db, "WF", "hi").await.expect_err("putStringDouble");
    assert!(val(&db, "WF").is_empty());
    caput(&db, "CMP", "hi").await.expect_err("putStringDouble");
    assert_eq!(field_f64(&db, "CMP", "NUSE"), 0.0);
}

/// A long-string field is `DBF_STRING` in its `cvt_dbaddr`
/// (`printfRecord.c:410-421`), so its row is `putStringString` — a byte copy,
/// never a parse. `caput PRF.VAL 7` must not become the number 7, and must not
/// be refused: `softIoc` takes it and `pp(TRUE)` then overwrites VAL from FMT.
#[epics_macros_rs::epics_test]
async fn a_long_string_buffer_takes_the_text_not_a_parse() {
    let db = build().await;
    caput(&db, "PRF", "hello").await.expect("putStringString");
    caput(&db, "PRF", "7").await.expect("still a byte copy");
}
