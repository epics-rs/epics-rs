//! R10-65 — scalcout fetches its string input links INAA..INLL.
//!
//! C `sCalcoutRecord.c::fetch_values` (890-941) is TWO loops: the numeric one
//! (INPA..INPL → A..L, `return`s at the first failure) and a string one
//! (INAA..INLL → AA..LL) with different rules — it reads DBR_STRING, it cannot
//! fail the calc gate (`return(0)`), a failed read writes the diagnostic
//! `"<record>:fetch(AA) failed"` INTO the field, and a multi-element
//! DBF_CHAR/DBF_UCHAR source is read as escaped text.
//!
//! The port had no INAA..INLL fields and never ran that loop, so AA..LL could
//! only ever hold what a client put into them.

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::record::Record;
use epics_base_rs::server::records::ai::AiRecord;
use epics_base_rs::server::records::scalcout::ScalcoutRecord;
use epics_base_rs::server::records::stringin::StringinRecord;
use epics_base_rs::server::records::waveform::WaveformRecord;
use epics_base_rs::types::{DbFieldType, EpicsValue};

async fn process(db: &PvDatabase, rec: &str) {
    let mut visited = epics_base_rs::server::database::ProcStack::new();
    db.process_record_with_links(rec, &mut visited)
        .await
        .unwrap();
}

async fn field(db: &PvDatabase, rec: &str, f: &str) -> EpicsValue {
    let inst = db.get_record(rec).unwrap();
    let g = inst.read();
    g.record.get_field(f).unwrap()
}

fn scalcout(calc: &str) -> ScalcoutRecord {
    let mut c = ScalcoutRecord::new();
    c.put_field("CALC", EpicsValue::String(calc.into()))
        .unwrap();
    c.special("CALC", true).unwrap();
    c
}

/// The base case the record could not do at all: INAA points at a string PV,
/// and the CALC reads it as AA.
#[epics_macros_rs::epics_test]
async fn r10_65_string_input_link_is_fetched_into_aa() {
    let db = PvDatabase::new();
    db.add_record("SSRC", Box::new(StringinRecord::new("hello")))
        .await
        .unwrap();

    let mut c = scalcout("AA");
    c.put_field("INAA", EpicsValue::String("SSRC".into()))
        .unwrap();
    db.add_record("SC", Box::new(c)).await.unwrap();

    process(&db, "SC").await;

    assert_eq!(
        field(&db, "SC", "AA").await,
        EpicsValue::String("hello".into()),
        "C fetch_values (:930) reads INAA as DBR_STRING into AA"
    );
    assert_eq!(
        field(&db, "SC", "SVAL").await,
        EpicsValue::String("hello".into()),
        "CALC=\"AA\" evaluates on the fetched string"
    );
}

/// The string loop's failure rule, which is NOT the numeric loop's: a string
/// link that does not resolve leaves the record computing (C returns 0 from the
/// string loop) and replaces the value field with the diagnostic text
/// (sCalcoutRecord.c:939-940).
#[epics_macros_rs::epics_test]
async fn r10_65_failed_string_link_writes_the_diagnostic_and_does_not_gate_the_calc() {
    let db = PvDatabase::new();
    db.add_record("NSRC", Box::new(AiRecord::new(7.0)))
        .await
        .unwrap();

    // A resolves (numeric loop), BB's link does not (string loop).
    let mut c = scalcout("A+1");
    c.put_field("INPA", EpicsValue::String("NSRC".into()))
        .unwrap();
    c.put_field("INBB", EpicsValue::String("NOSUCHREC".into()))
        .unwrap();
    c.put_field("BB", EpicsValue::String("stale".into()))
        .unwrap();
    db.add_record("SC", Box::new(c)).await.unwrap();

    process(&db, "SC").await;

    assert_eq!(
        field(&db, "SC", "BB").await,
        EpicsValue::String("SC:fetch(BB) failed".into()),
        "C epicsSnprintf(*psvalue, ..., \"%s:fetch(%s) failed\") replaces the value"
    );
    assert_eq!(
        field(&db, "SC", "VAL").await.to_f64().unwrap(),
        8.0,
        "the string loop returns 0 — a failed string link must NOT gate sCalcPerform"
    );
}

/// `epicsSnprintf(*psvalue, STRING_SIZE-1, ...)` (sCalcoutRecord.c:940) keeps
/// 38 characters of the diagnostic; the C oracle IOC gives
/// `SC_with_a_long_name_xx:fetch(AA) faile`.
#[epics_macros_rs::epics_test]
async fn a_long_record_name_cuts_the_fetch_diagnostic_at_38_characters() {
    let db = PvDatabase::new();
    let mut c = scalcout("AA");
    c.put_field("INAA", EpicsValue::String("NOSUCHREC".into()))
        .unwrap();
    db.add_record("SC_with_a_long_name_xx", Box::new(c))
        .await
        .unwrap();

    process(&db, "SC_with_a_long_name_xx").await;

    assert_eq!(
        field(&db, "SC_with_a_long_name_xx", "AA").await,
        EpicsValue::String("SC_with_a_long_name_xx:fetch(AA) faile".into()),
    );
}

/// Negative control for the gate: the SAME failure on a NUMERIC link does gate
/// the calc (`InputFetchPolicy::AbortOnFirstFailure`), so VAL freezes. The two
/// loops must not share a policy.
#[epics_macros_rs::epics_test]
async fn r10_65_failed_numeric_link_still_gates_the_calc() {
    let db = PvDatabase::new();

    let mut c = scalcout("A+1");
    c.put_field("INPA", EpicsValue::String("NOSUCHREC".into()))
        .unwrap();
    c.put_field("VAL", EpicsValue::Double(42.0)).unwrap();
    db.add_record("SC", Box::new(c)).await.unwrap();

    process(&db, "SC").await;

    assert_eq!(
        field(&db, "SC", "VAL").await.to_f64().unwrap(),
        42.0,
        "sCalcoutRecord.c:356 — a failed NUMERIC link freezes VAL"
    );
}

/// An unset INAA link is neither CA_LINK nor DB_LINK in C, so neither
/// `dbGetLink` branch runs, status stays 0, and AA keeps what was put to it.
#[epics_macros_rs::epics_test]
async fn r10_65_unset_string_link_leaves_the_field_alone() {
    let db = PvDatabase::new();

    let mut c = scalcout("AA");
    c.put_field("AA", EpicsValue::String("preset".into()))
        .unwrap();
    db.add_record("SC", Box::new(c)).await.unwrap();

    process(&db, "SC").await;

    assert_eq!(
        field(&db, "SC", "AA").await,
        EpicsValue::String("preset".into()),
        "no link configured — C leaves the string field untouched"
    );
}

/// C `sCalcoutRecord.c:914-931`: a DBF_CHAR/DBF_UCHAR source with more than one
/// element is the one type NOT read as DBR_STRING — C reads the array as text
/// and escapes it (`epicsStrSnPrintEscaped`). A DBR_STRING read of a char
/// waveform would have rendered element 0 as a number instead.
#[epics_macros_rs::epics_test]
async fn r10_65_char_waveform_source_is_read_as_escaped_text() {
    let db = PvDatabase::new();
    let mut wf = WaveformRecord::new(64, DbFieldType::Char);
    // "hi\tthere" — the tab must come back escaped as \t.
    wf.put_field(
        "VAL",
        EpicsValue::CharArray(b"hi\tthere".iter().map(|&b| b as i8 as u8).collect()),
    )
    .unwrap();
    db.add_record("WF", Box::new(wf)).await.unwrap();

    let mut c = scalcout("AA");
    c.put_field("INAA", EpicsValue::String("WF".into()))
        .unwrap();
    db.add_record("SC", Box::new(c)).await.unwrap();

    process(&db, "SC").await;

    assert_eq!(
        field(&db, "SC", "AA").await,
        EpicsValue::String("hi\\tthere".into()),
        "epicsStrSnPrintEscaped renders 0x09 as the two characters \\t"
    );
}

/// `epicsStrSnPrintEscaped(*psvalue, STRING_SIZE-1, ...)` (sCalcoutRecord.c:925)
/// keeps 38 characters and the NUL, and may cut an escape in half; a
/// DBR_STRING read fills the 40-byte field, 39 characters. The C oracle IOC
/// gives 38 x's, 37 x's and a lone backslash, and 39 characters.
#[epics_macros_rs::epics_test]
async fn r10_65_fetched_string_is_capped_at_the_c_field_width() {
    let db = PvDatabase::new();
    let long = "x".repeat(50);
    let mut wf = WaveformRecord::new(64, DbFieldType::Char);
    wf.put_field(
        "VAL",
        EpicsValue::CharArray(long.clone().into_bytes().into()),
    )
    .unwrap();
    db.add_record("WF", Box::new(wf)).await.unwrap();

    let mut c = scalcout("AA");
    c.put_field("INAA", EpicsValue::String("WF".into()))
        .unwrap();
    db.add_record("SC", Box::new(c)).await.unwrap();

    process(&db, "SC").await;

    assert_eq!(
        field(&db, "SC", "AA").await,
        EpicsValue::String("x".repeat(38).into()),
        "the STRING_SIZE-1 size argument leaves 38 characters"
    );

    let mut cut = WaveformRecord::new(64, DbFieldType::Char);
    cut.put_field(
        "VAL",
        EpicsValue::CharArray(format!("{}\\n", "x".repeat(37)).into_bytes().into()),
    )
    .unwrap();
    db.add_record("WFCUT", Box::new(cut)).await.unwrap();
    db.add_record("SLONG", Box::new(StringinRecord::new(&"y".repeat(39))))
        .await
        .unwrap();
    let mut c = scalcout("AA");
    c.put_field("INAA", EpicsValue::String("WFCUT".into()))
        .unwrap();
    c.put_field("INBB", EpicsValue::String("SLONG".into()))
        .unwrap();
    db.add_record("SC2", Box::new(c)).await.unwrap();

    process(&db, "SC2").await;

    assert_eq!(
        field(&db, "SC2", "AA").await,
        EpicsValue::String(format!("{}\\", "x".repeat(37)).into()),
        "the escaped backslash is cut after its first character"
    );
    assert_eq!(
        field(&db, "SC2", "BB").await,
        EpicsValue::String("y".repeat(39).into()),
        "a DBR_STRING read fills the field"
    );
}
