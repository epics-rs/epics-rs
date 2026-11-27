//! A DB link to a link field without `$` or a filter: `dbDbGetValue`
//! reads one element through `dbFastGetConvertRoutine`, which has no row
//! for a link type, and any other request through `dbGet`'s
//! `getLinkValue`. Pinned to oracleStdIoc (R7.0.10.1 + calc) for the same
//! records.

use std::time::Duration;

use epics_base_rs::server::database::{ProcStack, PvDatabase};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::records::{
    acalcout::AcalcoutRecord, scalcout::ScalcoutRecord, sseq::SseqRecord,
    transform::TransformRecord,
};
use epics_base_rs::types::EpicsValue;

const DB: &str = r#"
record(ai, "S") { }
record(ai, "LONG:ABCDEFGHIJKLMNOPQRSTUVWXYZ") { }
record(calc, "C") { field(INPA, "S NPP") field(INPB, "LONG:ABCDEFGHIJKLMNOPQRSTUVWXYZ NPP") field(INPD, "3.5") field(CALC, "A") }
record(stringin, "SI") { field(INP, "C.INPA NPP") }
record(lsi, "LS") { field(INP, "C.INPA NPP") field(SIZV, "40") }
record(ai, "AI") { field(INP, "C.INPD NPP") }
record(calc, "CD") { field(INPA, "C.INPD NPP") field(CALC, "A") }
record(ao, "AO") { field(DOL, "C.INPD NPP") field(OMSL, "closed_loop") }
record(waveform, "W1") { field(INP, "C.INPA NPP") field(FTVL, "CHAR") field(NELM, "1") }
record(aai, "AD") { field(INP, "C.INPD NPP") field(FTVL, "DOUBLE") field(NELM, "1") }
record(waveform, "WC") { field(INP, "C.INPA NPP") field(FTVL, "CHAR") field(NELM, "40") }
record(waveform, "WU") { field(INP, "C.INPB NPP") field(FTVL, "UCHAR") field(NELM, "8") }
record(waveform, "W2") { field(INP, "C.INPA NPP") field(FTVL, "CHAR") field(NELM, "2") }
record(waveform, "WD") { field(INP, "C.INPA NPP") field(FTVL, "DOUBLE") field(NELM, "5") }
record(waveform, "WS") { field(INP, "C.INPA NPP") field(FTVL, "STRING") field(NELM, "3") }
record(waveform, "WL") { field(INP, "C.INPA NPP") field(FTVL, "LONG") field(NELM, "3") }
record(waveform, "WE") { field(INP, "C.INPE NPP") field(FTVL, "CHAR") field(NELM, "40") }
record(subArray, "SA") { field(INP, "C.INPD NPP") field(FTVL, "CHAR") field(MALM, "8") field(NELM, "8") }
record(stringin, "FS") { field(INP, "C.INPD{\"ts\":{}} NPP") }
record(ai, "FA") { field(INP, "C.INPD{\"ts\":{}} NPP") }
record(stringin, "DS") { field(INP, "C.INPA$ NPP") }
record(waveform, "DW") { field(INP, "C.INPD$ NPP") field(FTVL, "CHAR") field(NELM, "8") }
record(aSub, "AS") { field(INPA, "C.INPA NPP") field(FTA, "CHAR") field(NOA, "40") field(INPB, "C.INPD NPP") field(FTB, "DOUBLE") field(INPC, "C.INPA NPP") field(FTC, "STRING") field(INPD, "C.INPA NPP") field(FTD, "CHAR") field(NOD, "1") }
record(sseq, "SQ") { field(DOL1, "C.INPA NPP") field(DOL2, "C.INPD NPP") }
record(scalcout, "SC") { field(INAA, "C.INPA NPP") field(CALC, "AA") }
record(acalcout, "AC") { field(INAA, "C.INPA NPP") field(NELM, "40") field(CALC, "A") }
record(acalcout, "AN") { field(INPA, "C.INPD NPP") field(CALC, "A") }
record(transform, "TR") { field(INPA, "C.INPD NPP") field(CLCB, "A") }
"#;

/// Until `rec` leaves PACT: sseq runs even DLY 0 links from a callback
/// (`sseqRecord.c:443-448`), after its process returns.
async fn idle(db: &PvDatabase, rec: &str) {
    let cell = db.get_record(rec).unwrap();
    epics_base_rs::runtime::task::timeout(fact_budget::FACT_BUDGET, async {
        while cell.read().is_processing() {
            epics_base_rs::runtime::task::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{rec} never left PACT"));
}

/// The text up to its NUL, the bytes of a UCHAR array, the first element
/// of another array, or a number.
fn render(v: EpicsValue) -> String {
    let text = |b: &[u8]| String::from_utf8(b.iter().copied().take_while(|&c| c != 0).collect());
    match v {
        EpicsValue::String(s) => s.as_str_lossy().into_owned(),
        EpicsValue::CharArray(b) => text(&b).unwrap(),
        EpicsValue::UCharArray(b) => format!("{:?}", &b[..]),
        EpicsValue::StringArray(a) => a
            .first()
            .map_or_else(|| "[]".into(), |s| s.as_str_lossy().into_owned()),
        EpicsValue::DoubleArray(a) => a.first().map_or_else(|| "[]".into(), |f| format!("{f:?}")),
        v => v
            .to_f64()
            .map_or_else(|| format!("{v:?}"), |f| format!("{f:?}")),
    }
}

#[epics_macros_rs::epics_test]
async fn a_bare_link_field_refuses_one_element_and_reads_its_text_otherwise() {
    let db = IocBuilder::new()
        .register_record_type("sseq", || Box::new(SseqRecord::default()))
        .register_record_type("scalcout", || Box::new(ScalcoutRecord::default()))
        .register_record_type("acalcout", || Box::new(AcalcoutRecord::default()))
        .register_record_type("transform", || Box::new(TransformRecord::default()))
        .db_string(DB, &std::collections::HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;
    for rec in [
        "SI", "LS", "AI", "CD", "AO", "W1", "AD", "WC", "WU", "W2", "WD", "WS", "WL", "WE", "SA",
        "FS", "FA", "DS", "DW", "AS", "SQ", "SC", "AC", "AN", "TR",
    ] {
        db.process_record_with_links(rec, &mut ProcStack::new())
            .await
            .or_else(epics_base_rs::error::CaError::unless_process_status)
            .unwrap();
    }
    idle(&db, "SQ").await;
    // SEVR 3 is INVALID; STAT 14 is LINK_ALARM, 17 UDF_ALARM.
    let want = [
        // One element, `dbDbGetValue`'s fast path: S_db_badDbrtype.
        ("SI.VAL", ""),
        ("SI.SEVR", "3.0"),
        ("SI.STAT", "14.0"),
        ("LS.LEN", "0.0"),
        ("LS.SEVR", "3.0"),
        ("LS.STAT", "14.0"),
        ("AI.VAL", "0.0"),
        ("AI.SEVR", "3.0"),
        ("AI.STAT", "14.0"),
        ("CD.VAL", "0.0"),
        ("CD.SEVR", "3.0"),
        ("CD.STAT", "14.0"),
        ("AO.VAL", "0.0"),
        ("AO.SEVR", "3.0"),
        ("AO.STAT", "14.0"),
        ("W1.NORD", "0.0"),
        ("W1.SEVR", "3.0"),
        ("W1.STAT", "14.0"),
        ("AD.SEVR", "3.0"),
        ("AD.STAT", "14.0"),
        // Several elements, `getLinkValue`: DBR_CHAR/UCHAR text truncated
        // with its NUL, DBR_STRING text, DBR_DOUBLE NaN, others refused.
        ("WC.VAL", "S NPP NMS"),
        ("WC.NORD", "10.0"),
        ("WC.SEVR", "0.0"),
        ("WU.VAL", "[76, 79, 78, 71, 58, 65, 66, 0]"),
        ("WU.NORD", "8.0"),
        ("WU.SEVR", "0.0"),
        ("W2.VAL", "S"),
        ("W2.NORD", "2.0"),
        ("W2.SEVR", "0.0"),
        ("WD.VAL", "NaN"),
        ("WD.NORD", "1.0"),
        ("WD.SEVR", "0.0"),
        ("WS.VAL", "S NPP NMS"),
        ("WS.NORD", "1.0"),
        ("WS.SEVR", "0.0"),
        ("WL.NORD", "0.0"),
        ("WL.SEVR", "3.0"),
        ("WL.STAT", "14.0"),
        ("WE.VAL", ""),
        ("WE.NORD", "1.0"),
        ("WE.SEVR", "0.0"),
        ("SA.VAL", "3.5"),
        ("SA.NORD", "4.0"),
        ("SA.SEVR", "0.0"),
        // A filter leaves the fast path: `getLinkValue` at any count.
        ("FS.VAL", "3.5"),
        ("FS.SEVR", "0.0"),
        ("FA.VAL", "NaN"),
        ("FA.SEVR", "3.0"),
        ("FA.STAT", "17.0"),
        // `$` never takes it.
        ("DS.VAL", "S NPP NMS"),
        ("DS.SEVR", "0.0"),
        ("DW.VAL", "3.5"),
        ("DW.NORD", "4.0"),
        ("DW.SEVR", "0.0"),
        // Each reader's own request: NOA 40 reads the text, the rest refuse.
        ("AS.A", "S NPP NMS"),
        ("AS.NEA", "10.0"),
        ("AS.B", "0.0"),
        ("AS.C", ""),
        ("AS.D", "0.0"),
        ("AS.SEVR", "3.0"),
        ("AS.STAT", "14.0"),
        ("SQ.STR1", "0"),
        ("SQ.DO1", "0.0"),
        ("SQ.STR2", "0"),
        ("SQ.DO2", "0.0"),
        ("SQ.SEVR", "3.0"),
        ("SQ.STAT", "14.0"),
        // INAA of one element reads DBR_STRING, of NELM DBR_DOUBLE.
        ("SC.AA", "SC:fetch(AA) failed"),
        ("SC.SVAL", "SC:fetch(AA) failed"),
        ("SC.SEVR", "3.0"),
        ("SC.STAT", "14.0"),
        ("AC.AA", "NaN"),
        ("AC.NUSE", "0.0"),
        ("AC.VAL", "0.0"),
        ("AC.SEVR", "0.0"),
        ("AN.A", "0.0"),
        ("AN.SEVR", "3.0"),
        ("AN.STAT", "14.0"),
        ("TR.A", "0.0"),
        ("TR.SEVR", "3.0"),
        ("TR.STAT", "14.0"),
    ];
    let got: Vec<_> = want
        .iter()
        .map(|(pv, _)| (*pv, render(db.get_pv(pv).unwrap())))
        .collect();
    let want: Vec<_> = want.iter().map(|(pv, v)| (*pv, v.to_string())).collect();
    assert_eq!(got, want);
}
