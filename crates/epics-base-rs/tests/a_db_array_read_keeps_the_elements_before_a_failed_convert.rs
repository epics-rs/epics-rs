//! A DB link read with an element count converts through `dbGet`'s
//! `dbGetConvertRoutine` (dbAccess.c:1007-1021) straight into the reader's
//! buffer. `getString<Type>` (dbConvert.c) stops at the first string it
//! cannot parse: the elements before it are written, the rest of the buffer
//! keeps what it held, and `dbGet` returns the status, so `dbGetLink` raises
//! LINK/INVALID. Every value below is the C oracle IOC's, on the same db.

use std::collections::HashMap;

use epics_base_rs::error::CaError;
use epics_base_rs::server::database::{ProcStack, PvDatabase};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::records::acalcout::AcalcoutRecord;
use epics_base_rs::types::{DbFieldType, EpicsValue, PvString};

const DB: &str = r#"
record(stringin, "SR") { field(VAL, "abc") }
record(waveform, "SW") { field(FTVL, "STRING") field(NELM, "3") }
record(waveform, "SC") { field(FTVL, "STRING") field(NELM, "3") }
record(ai, "AISD") { field(INP, "SR") }
record(waveform, "WFD") { field(NELM, "4") field(FTVL, "DOUBLE") field(INP, "SW") }
record(waveform, "WFDL") { field(NELM, "4") field(FTVL, "LONG") field(INP, "SW") }
record(waveform, "WFS") { field(NELM, "4") field(FTVL, "DOUBLE") field(INP, "SR") }
record(waveform, "WSD") { field(NELM, "4") field(FTVL, "DOUBLE") field(SIMM, "YES") field(SIOL, "SW") }
record(aai, "AID") { field(NELM, "4") field(FTVL, "DOUBLE") field(INP, "SW") }
record(aai, "AISI") { field(NELM, "4") field(FTVL, "DOUBLE") field(SIMM, "YES") field(SIOL, "SW") }
record(subArray, "SAD") { field(MALM, "4") field(NELM, "2") field(INDX, "1") field(FTVL, "DOUBLE") field(INP, "SW") }
record(aao, "AOD") { field(NELM, "4") field(FTVL, "DOUBLE") field(OMSL, "closed_loop") field(DOL, "SW") }
record(compress, "CPD") { field(ALG, "Circular Buffer") field(NSAM, "4") field(INP, "SW") }
record(aSub, "ASD") { field(INPA, "SW") field(FTA, "DOUBLE") field(NOA, "4") }
record(acalcout, "ACD") { field(NELM, "4") field(INAA, "SW") field(CALC, "AA") }
record(printf, "PFD") { field(FMT, "%ls") field(INP0, "SC") field(SIZV, "40") }
record(waveform, "WFE") { field(NELM, "4") field(FTVL, "ENUM") field(INP, "SW") }
record(aSub, "ASE") { field(INPA, "SW") field(FTA, "ENUM") field(NOA, "4") }
"#;

const READERS: &[&str] = &[
    "AISD", "WFD", "WFDL", "WFS", "WSD", "AID", "AISI", "SAD", "AOD", "CPD", "ASD", "ACD", "PFD",
    "WFE", "ASE",
];

fn strings(items: &[&str]) -> EpicsValue {
    EpicsValue::StringArray(items.iter().map(|s| PvString::from(*s)).collect())
}

fn doubles(items: &[f64]) -> EpicsValue {
    EpicsValue::DoubleArray(items.to_vec().into())
}

/// The C string `pv`'s chars hold.
fn text(db: &PvDatabase, pv: &str) -> String {
    match db.get_pv(pv).unwrap() {
        EpicsValue::CharArray(b) => {
            let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
            String::from_utf8_lossy(&b[..end]).into_owned()
        }
        other => panic!("{pv} is {other:?}"),
    }
}

/// Every element `pv` serves.
fn nums(db: &PvDatabase, pv: &str) -> Vec<f64> {
    match db.get_pv(pv).unwrap().get_convert(DbFieldType::Double) {
        Ok(EpicsValue::DoubleArray(a)) => a.to_vec(),
        Ok(EpicsValue::Double(x)) => vec![x],
        other => panic!("{pv} is {other:?}"),
    }
}

/// `dbpf`: a put to a pp field processes the record.
async fn dbpf(db: &PvDatabase, rec: &str, field: &str, value: EpicsValue) {
    db.put_record_field_from_ca(rec, field, value)
        .await
        .unwrap();
}

async fn process(db: &PvDatabase, rec: &str) {
    db.process_record_with_links(rec, &mut ProcStack::new())
        .await
        .or_else(CaError::unless_process_status)
        .unwrap();
}

/// `(pv, C's value)`, numbers as f64; an array is every element served.
type Want = &'static [(&'static str, &'static [f64])];

/// SW = ["1","2","3"]; SR = "abc" fails every round.
const VALID: Want = &[
    ("AISD", &[0.0]),
    ("AISD.SEVR", &[3.0]),
    ("WFD", &[1.0, 2.0, 3.0]),
    ("WFD.SEVR", &[0.0]),
    ("WFDL", &[1.0, 2.0, 3.0]),
    ("WFS", &[1.0, 2.0, 3.0, 4.0]),
    ("WFS.SEVR", &[3.0]),
    ("WSD", &[1.0, 2.0, 3.0]),
    ("WSD.SEVR", &[0.0]),
    ("AID", &[1.0, 2.0, 3.0]),
    ("AISI", &[1.0, 2.0, 3.0]),
    ("SAD", &[2.0, 3.0]),
    ("AOD", &[1.0, 2.0, 3.0]),
    ("CPD", &[1.0, 2.0, 3.0]),
    ("CPD.SEVR", &[0.0]),
    ("ASD.A", &[1.0, 2.0, 3.0]),
    ("ASD.NEA", &[3.0]),
    ("ASD.SEVR", &[0.0]),
    ("ACD.AA", &[1.0, 2.0, 3.0, 0.0]),
    ("ACD.SEVR", &[0.0]),
    ("PFD.SEVR", &[0.0]),
    ("WFE", &[1.0, 2.0, 3.0]),
    ("WFE.SEVR", &[0.0]),
    ("ASE.A", &[1.0, 2.0, 3.0]),
    ("ASE.NEA", &[3.0]),
];

/// SW = ["7","abc","9"]: element 0 is written, the rest of each buffer is
/// what the valid round left there, and the read fails.
const FAILING: Want = &[
    ("AISD", &[0.0]),
    ("AISD.SEVR", &[3.0]),
    ("WFD", &[7.0, 2.0, 3.0]),
    ("WFD.NORD", &[3.0]),
    ("WFD.SEVR", &[3.0]),
    ("WFD.STAT", &[14.0]),
    ("WFDL", &[7.0, 2.0, 3.0]),
    ("WFDL.SEVR", &[3.0]),
    ("WFS", &[1.0, 2.0, 3.0, 4.0]),
    ("WFS.SEVR", &[3.0]),
    ("WSD", &[7.0, 2.0, 3.0]),
    ("WSD.NORD", &[3.0]),
    ("WSD.SEVR", &[3.0]),
    ("WSD.STAT", &[14.0]),
    ("AID", &[7.0, 2.0, 3.0]),
    ("AID.SEVR", &[3.0]),
    ("AISI", &[7.0, 2.0, 3.0]),
    ("AISI.SEVR", &[3.0]),
    ("SAD", &[]),
    ("SAD.NORD", &[2.0]),
    ("SAD.SEVR", &[3.0]),
    ("AOD", &[7.0, 2.0, 3.0]),
    ("AOD.NORD", &[3.0]),
    ("AOD.SEVR", &[0.0]),
    ("CPD", &[1.0, 2.0, 3.0]),
    ("CPD.SEVR", &[3.0]),
    ("ASD.A", &[7.0, 2.0, 3.0]),
    ("ASD.NEA", &[3.0]),
    ("ASD.SEVR", &[3.0]),
    ("ACD.AA", &[7.0, 2.0, 3.0, 0.0]),
    ("ACD.SEVR", &[3.0]),
    ("ACD.STAT", &[14.0]),
    ("PFD.SEVR", &[3.0]),
    ("WFE", &[7.0, 2.0, 3.0]),
    ("WFE.NORD", &[3.0]),
    ("WFE.SEVR", &[3.0]),
    ("WFE.STAT", &[14.0]),
    ("ASE.A", &[7.0, 2.0, 3.0]),
    ("ASE.NEA", &[3.0]),
    ("ASE.SEVR", &[3.0]),
    ("ASE.STAT", &[14.0]),
];

/// SW = ["8","xyz"]: two elements. A failed read leaves NORD as it was,
/// except the simulated waveform's: `waveformRecord.c:355-357` stores the
/// count `dbGet` clamped (dbAccess.c:999-1003) whatever the status.
const SHORT: Want = &[
    ("WFD", &[8.0, 2.0, 3.0]),
    ("WFD.NORD", &[3.0]),
    ("WFD.SEVR", &[3.0]),
    ("WFDL", &[8.0, 2.0, 3.0]),
    ("WSD", &[8.0, 2.0]),
    ("WSD.NORD", &[2.0]),
    ("WSD.SEVR", &[3.0]),
    ("AID", &[8.0, 2.0, 3.0]),
    ("AID.NORD", &[3.0]),
    ("AISI", &[8.0, 2.0, 3.0]),
    ("AISI.NORD", &[3.0]),
    ("SAD", &[]),
    ("SAD.NORD", &[2.0]),
    ("AOD", &[8.0, 2.0, 3.0]),
    ("AOD.NORD", &[3.0]),
    ("AOD.SEVR", &[0.0]),
    ("CPD", &[1.0, 2.0, 3.0]),
    ("CPD.SEVR", &[3.0]),
    ("ASD.A", &[8.0, 2.0, 3.0]),
    ("ASD.NEA", &[3.0]),
    ("ACD.AA", &[8.0, 2.0, 3.0, 0.0]),
    ("ACD.SEVR", &[3.0]),
    ("WFE", &[8.0, 2.0, 3.0]),
    ("WFE.NORD", &[3.0]),
    ("ASE.A", &[8.0, 2.0, 3.0]),
    ("ASE.NEA", &[3.0]),
];

/// Each `pv` that does not serve C's value, as a line; an `FTVL`/`FTA`
/// ENUM buffer stays `DBF_ENUM` whatever the read left in it.
fn check(db: &PvDatabase, round: &str, want: Want) -> Vec<String> {
    let mut wrong: Vec<String> = want
        .iter()
        .filter_map(|&(pv, c)| {
            let got = nums(db, pv);
            (got != c).then(|| format!("{round} {pv}: rs {got:?}, C {c:?}"))
        })
        .collect();
    for pv in ["WFE", "ASE.A"] {
        let got = db.get_pv(pv).unwrap().db_field_type();
        if got != DbFieldType::Enum {
            wrong.push(format!("{round} {pv}: rs {got:?}, C Enum"));
        }
    }
    wrong
}

fn check_text(db: &PvDatabase, round: &str, pv: &str, c: &str) -> Vec<String> {
    let got = text(db, pv);
    if got == c {
        return Vec::new();
    }
    vec![format!("{round} {pv}: rs {got:?}, C {c:?}")]
}

async fn process_readers(db: &PvDatabase) {
    for rec in READERS {
        process(db, rec).await;
    }
}

#[epics_macros_rs::epics_test]
async fn a_db_array_read_writes_the_elements_before_the_failed_one_and_fails() {
    let (db, _h) = IocBuilder::new()
        .register_record_type("acalcout", || Box::new(AcalcoutRecord::default()))
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    db.initialize_link_locality().await;
    db.scan_control().run();

    let four = doubles(&[1.0, 2.0, 3.0, 4.0]);
    for rec in ["WFD", "WFDL", "WFS", "WSD", "AID", "AISI", "AOD"] {
        dbpf(&db, rec, "VAL", four.clone()).await;
    }
    dbpf(&db, "ACD", "AA", four.clone()).await;
    dbpf(&db, "ASD", "A", four.clone()).await;
    dbpf(&db, "ASE", "A", four).await;
    dbpf(&db, "SAD", "VAL", doubles(&[1.0, 2.0])).await;

    dbpf(&db, "SW", "VAL", strings(&["1", "2", "3"])).await;
    dbpf(&db, "SC", "VAL", strings(&["72", "105"])).await;
    process_readers(&db).await;
    let mut wrong = check(&db, "valid", VALID);
    wrong.extend(check_text(&db, "valid", "PFD", "Hi"));

    dbpf(&db, "SW", "VAL", strings(&["7", "abc", "9"])).await;
    dbpf(&db, "SC", "VAL", strings(&["72", "abc", "33"])).await;
    process_readers(&db).await;
    wrong.extend(check(&db, "failing", FAILING));
    wrong.extend(check_text(&db, "failing", "PFD", "LNK"));

    dbpf(&db, "SW", "VAL", strings(&["8", "xyz"])).await;
    process_readers(&db).await;
    wrong.extend(check(&db, "short", SHORT));
    assert!(wrong.is_empty(), "\n{}", wrong.join("\n"));
}
