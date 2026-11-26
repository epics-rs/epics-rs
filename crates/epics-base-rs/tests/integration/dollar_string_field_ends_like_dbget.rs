//! A DB link to `$` of a `DBF_STRING` field reads the field's char log
//! through the reader's own `dbGet`, which ends a `DBR_CHAR` or `DBR_UCHAR`
//! read with a NUL in the last element asked for and counts a `DBR_CHAR`
//! one `strlen + 1` (`dbConvert.c:439-480`, `dbAccess.c:1037-1044`); a link
//! or an attribute `$` is its text cut at the request. Pinned to
//! oracleStdIoc (R7.0.10.1 + calc) for the same records.

use std::time::Duration;

use epics_base_rs::server::database::{ProcStack, PvDatabase};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::records::sseq::SseqRecord;
use epics_base_rs::types::EpicsValue;

const DB: &str = r#"
record(calc, "SRC") { field(DESC, "abcdefgh") field(INPA, "12345") }
record(waveform, "U4") { field(INP, "SRC.DESC$ NPP") field(FTVL, "UCHAR") field(NELM, "4") }
record(waveform, "C4") { field(INP, "SRC.DESC$ NPP") field(FTVL, "CHAR") field(NELM, "4") }
record(waveform, "U12") { field(INP, "SRC.DESC$ NPP") field(FTVL, "UCHAR") field(NELM, "12") }
record(waveform, "C12") { field(INP, "SRC.DESC$ NPP") field(FTVL, "CHAR") field(NELM, "12") }
record(waveform, "S12") { field(INP, "SRC.DESC$ NPP") field(FTVL, "SHORT") field(NELM, "12") }
record(waveform, "C1") { field(INP, "SRC.DESC$ NPP") field(FTVL, "CHAR") field(NELM, "1") }
record(stringin, "SI") { field(INP, "SRC.DESC$ NPP") }
record(lsi, "LSI") { field(INP, "SRC.DESC$ NPP") field(SIZV, "5") }
record(aai, "A4") { field(INP, "SRC.DESC$ NPP") field(FTVL, "CHAR") field(NELM, "4") }
record(subArray, "SA4") { field(INP, "SRC.DESC$ NPP") field(FTVL, "CHAR") field(MALM, "4") field(NELM, "4") }
record(aSub, "AS") {
  field(INPA, "SRC.DESC$ NPP") field(FTA, "CHAR") field(NOA, "4")
  field(INPB, "SRC.DESC$ NPP") field(FTB, "UCHAR") field(NOB, "12")
  field(INPC, "SRC.DESC$ NPP") field(FTC, "CHAR") field(NOC, "12")
  field(INPD, "SRC.INPA$ NPP") field(FTD, "CHAR") field(NOD, "4")
  field(INPE, "SRC.RTYP$ NPP") field(FTE, "CHAR") field(NOE, "3")
  field(INPF, "SRC.DESC$ NPP") field(FTF, "UCHAR") field(NOF, "4")
}
"#;

/// A char array as its text up to the NUL (its signed elements when that
/// is not text), any other array as its elements, a scalar as a number.
fn render(v: EpicsValue) -> String {
    match v {
        EpicsValue::String(s) => s.as_str_lossy().into_owned(),
        EpicsValue::CharArray(b) => {
            String::from_utf8(b.iter().copied().take_while(|&c| c != 0).collect())
                .unwrap_or_else(|_| format!("{:?}", b.iter().map(|&c| c as i8).collect::<Vec<_>>()))
        }
        EpicsValue::UCharArray(b) => format!("{:?}", &b[..]),
        EpicsValue::ShortArray(a) => format!("{:?}", &a[..]),
        EpicsValue::LongArray(a) => format!("{:?}", &a[..]),
        v => v
            .to_f64()
            .map_or_else(|| format!("{v:?}"), |f| format!("{f:?}")),
    }
}

#[epics_macros_rs::epics_test]
async fn a_dollar_string_read_ends_at_the_readers_request() {
    let db = IocBuilder::new()
        .db_string(DB, &std::collections::HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;
    for rec in [
        "U4", "C4", "U12", "C12", "S12", "C1", "SI", "LSI", "A4", "SA4", "AS",
    ] {
        db.process_record_with_links(rec, &mut ProcStack::new())
            .await
            .or_else(epics_base_rs::error::CaError::unless_process_status)
            .unwrap();
    }
    let padded = "[97, 98, 99, 100, 101, 102, 103, 104, 0, 0, 0, 0]";
    let want = [
        ("U4.VAL", "[97, 98, 99, 0]"),
        ("U4.NORD", "4.0"),
        ("C4.VAL", "abc"),
        ("C4.NORD", "4.0"),
        // The log is the field's whole zero-padded buffer, not its text.
        ("U12.VAL", padded),
        ("U12.NORD", "12.0"),
        ("C12.VAL", "abcdefgh"),
        ("C12.NORD", "9.0"),
        ("S12.VAL", padded),
        ("S12.NORD", "12.0"),
        ("C1.VAL", ""),
        ("C1.NORD", "1.0"),
        ("SI.VAL", "97"),
        ("LSI.VAL", "abcdefgh"),
        ("LSI.LEN", "9.0"),
        ("A4.VAL", "abc"),
        ("A4.NORD", "4.0"),
        ("SA4.VAL", "abc"),
        ("SA4.NORD", "4.0"),
        ("AS.A", "abc"),
        ("AS.NEA", "4.0"),
        ("AS.B", padded),
        ("AS.NEB", "12.0"),
        ("AS.C", "abcdefgh"),
        ("AS.NEC", "9.0"),
        ("AS.D", "123"),
        ("AS.NED", "4.0"),
        ("AS.E", "ca"),
        ("AS.NEE", "3.0"),
        ("AS.F", "[97, 98, 99, 0]"),
        ("AS.NEF", "4.0"),
        ("U4.SEVR", "0.0"),
        ("C4.SEVR", "0.0"),
        ("U12.SEVR", "0.0"),
        ("C12.SEVR", "0.0"),
        ("S12.SEVR", "0.0"),
        ("C1.SEVR", "0.0"),
        ("SI.SEVR", "0.0"),
        ("LSI.SEVR", "0.0"),
        ("A4.SEVR", "0.0"),
        ("SA4.SEVR", "0.0"),
        ("AS.SEVR", "0.0"),
    ];
    let want: Vec<_> = want.iter().map(|(pv, v)| (*pv, v.to_string())).collect();
    let got: Vec<_> = want
        .iter()
        .map(|(pv, _)| (*pv, render(db.get_pv(pv).unwrap())))
        .collect();
    assert_eq!(got, want);
}

/// The same ending on a filtered log, through sseq's and printf's 40- and
/// `SIZV`-character requests, and on a link whose target is first resolved
/// at the read. C refuses a `${` filter in a `.db` file ("Has unexpanded
/// macro"), so the `ts` links are put after init, as in the oracle run.
const FILTERED_DB: &str = r#"
record(calc, "SRC") { field(DESC, "abcdefgh") }
record(calc, "SRC40") { field(DESC, "0123456789abcdefghijklmnopqrstuvwxyzABCD") field(INPA, "1234567890123456789012345678901234567890123456789") }
record(waveform, "TC1") { field(FTVL, "CHAR") field(NELM, "1") }
record(waveform, "TC4") { field(FTVL, "CHAR") field(NELM, "4") }
record(waveform, "TU4") { field(FTVL, "UCHAR") field(NELM, "4") }
record(waveform, "TE4") { field(FTVL, "CHAR") field(NELM, "4") }
record(waveform, "TS4") { field(FTVL, "CHAR") field(NELM, "4") }
record(waveform, "TL4") { field(FTVL, "LONG") field(NELM, "4") }
record(waveform, "AC30") { field(INP, "SRC.DESC$[2:5] NPP") field(FTVL, "CHAR") field(NELM, "30") }
record(waveform, "AU30") { field(INP, "SRC.DESC$[2:5] NPP") field(FTVL, "UCHAR") field(NELM, "30") }
record(waveform, "AC2") { field(INP, "SRC.DESC$[2:5] NPP") field(FTVL, "CHAR") field(NELM, "2") }
record(waveform, "AONE") { field(INP, "SRC.DESC$[0:0] NPP") field(FTVL, "CHAR") field(NELM, "30") }
record(waveform, "AIC") { field(INP, "SRC.DESC$[0:2:-1] NPP") field(FTVL, "CHAR") field(NELM, "30") }
record(waveform, "AIU") { field(INP, "SRC.DESC$[0:2:-1] NPP") field(FTVL, "UCHAR") field(NELM, "30") }
record(waveform, "ANC") { field(INP, "SRC.DESC$[-36:-1] NPP") field(FTVL, "CHAR") field(NELM, "30") }
record(waveform, "AEC") { field(INP, "SRC.DESC$[5:2] NPP") field(FTVL, "CHAR") field(NELM, "30") }
record(waveform, "AEU") { field(INP, "SRC.DESC$[5:2] NPP") field(FTVL, "UCHAR") field(NELM, "30") }
record(sseq, "SQ") { field(DOL1, "SRC40.DESC$ NPP") field(DOL2, "SRC40.INPA$ NPP") field(DOL3, "SRC.DESC$ NPP") }
record(printf, "PF1") { field(FMT, "%ls") field(INP0, "SRC40.DESC$ NPP") }
record(printf, "PF2") { field(FMT, "%ls") field(INP0, "SRC40.INPA$ NPP") }
record(printf, "PF3") { field(FMT, "%ls") field(INP0, "SRC40.INPA$ NPP") field(SIZV, "100") }
record(printf, "PF5") { field(FMT, "%ls") field(INP0, "SRC40.DESC$ NPP") field(SIZV, "16") }
record(waveform, "RL") { field(FTVL, "CHAR") field(NELM, "4") }
"#;

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

async fn process(db: &PvDatabase, rec: &str) {
    db.process_record_with_links(rec, &mut ProcStack::new())
        .await
        .or_else(epics_base_rs::error::CaError::unless_process_status)
        .unwrap();
}

#[epics_macros_rs::epics_test]
async fn a_filtered_or_capped_dollar_string_read_ends_like_dbget() {
    let db = IocBuilder::new()
        .register_record_type("sseq", || Box::new(SseqRecord::default()))
        .db_string(FILTERED_DB, &std::collections::HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;
    // The source never processed: TIME is 0, so `epoch unix` gives
    // [631152000, 0], whose low byte is 0x80.
    let unix = r#"SRC.DESC${"ts":{"num":"ts","epoch":"unix"}} NPP"#;
    for (rec, inp) in [
        ("TC1", unix),
        ("TC4", unix),
        ("TU4", unix),
        ("TE4", r#"SRC.DESC${"ts":{"num":"ts"}} NPP"#),
        ("TS4", r#"SRC.DESC${"ts":{"num":"sec","epoch":"unix"}} NPP"#),
        ("TL4", unix),
    ] {
        db.put_pv_no_process(&format!("{rec}.INP"), EpicsValue::String(inp.into()))
            .await
            .unwrap();
    }
    // RL reads once unlinked, then through a link put after that read.
    process(&db, "RL").await;
    db.put_pv_no_process("RL.INP", EpicsValue::String("SRC.DESC$ NPP".into()))
        .await
        .unwrap();
    for rec in [
        "TC1", "TC4", "TU4", "TE4", "TS4", "TL4", "AC30", "AU30", "AC2", "AONE", "AIC", "AIU",
        "ANC", "AEC", "AEU", "SQ", "PF1", "PF2", "PF3", "PF5", "RL",
    ] {
        process(&db, rec).await;
    }
    idle(&db, "SQ").await;
    let digits = "1234567890123456789012345678901234567890123456789";
    let aiu = format!("[97, 99, 101, 103{}]", ", 0".repeat(17));
    let want = [
        // The `ts` log is two ULONGs; `dbGet`'s DBR_CHAR tail still ends it.
        ("TC1.VAL", ""),
        ("TC1.NORD", "1.0"),
        ("TC4.VAL", "[-128, 0]"),
        ("TC4.NORD", "2.0"),
        ("TU4.VAL", "[128, 0]"),
        ("TU4.NORD", "2.0"),
        ("TE4.VAL", ""),
        ("TE4.NORD", "1.0"),
        // One element: `dbGet`'s fast path, no tail.
        ("TS4.VAL", "[-128]"),
        ("TS4.NORD", "1.0"),
        ("TL4.VAL", "[631152000, 0]"),
        ("TL4.NORD", "2.0"),
        ("AC30.VAL", "cde"),
        ("AC30.NORD", "4.0"),
        ("AU30.VAL", "[99, 100, 101, 0]"),
        ("AU30.NORD", "4.0"),
        ("AC2.VAL", "c"),
        ("AC2.NORD", "2.0"),
        ("AONE.VAL", "a"),
        ("AONE.NORD", "1.0"),
        ("AIC.VAL", "aceg"),
        ("AIC.NORD", "5.0"),
        ("AIU.VAL", &aiu),
        ("AIU.NORD", "21.0"),
        ("ANC.VAL", "fgh"),
        ("ANC.NORD", "4.0"),
        ("AEC.VAL", ""),
        ("AEC.NORD", "1.0"),
        ("AEU.VAL", "[]"),
        ("AEU.NORD", "0.0"),
        ("SQ.STR1", "0123456789abcdefghijklmnopqrstuvwxyzABC"),
        ("SQ.STR2", &digits[..39]),
        ("SQ.STR3", "abcdefgh"),
        ("PF1.VAL", "0123456789abcdefghijklmnopqrstuvwxyzABCD"),
        ("PF1.LEN", "41.0"),
        ("PF2.VAL", &digits[..40]),
        ("PF2.LEN", "41.0"),
        ("PF3.VAL", digits),
        ("PF3.LEN", "50.0"),
        ("PF5.VAL", "0123456789abcde"),
        ("PF5.LEN", "16.0"),
        ("RL.VAL", "abc"),
        ("RL.NORD", "4.0"),
    ];
    let mut want: Vec<_> = want
        .iter()
        .map(|(pv, v)| (pv.to_string(), v.to_string()))
        .collect();
    for rec in [
        "TC1", "TC4", "TU4", "TE4", "TS4", "TL4", "AC30", "AU30", "AC2", "AONE", "AIC", "AIU",
        "ANC", "AEC", "AEU", "SQ", "PF1", "PF2", "PF3", "PF5", "RL",
    ] {
        want.push((format!("{rec}.SEVR"), "0.0".to_string()));
    }
    let got: Vec<_> = want
        .iter()
        .map(|(pv, _)| (pv.clone(), render(db.get_pv(pv).unwrap())))
        .collect();
    assert_eq!(got, want);
}

/// printf's `%.Nls` reads `N + 1` chars (`printfRecord.c:231-246`), so a
/// `$` string link ends at `N` and a constant loads `N` through
/// `dbLoadLinkLS`'s `size - 1` cut (`lnkConst.c:445`,
/// `dbConvertJSON.c:100`), the cut lsi's and lso's SIZV loads take too.
const PRINTF_DB: &str = r#"
record(calc, "SRC") { field(DESC, "abcdefgh") }
record(calc, "SRC40") { field(DESC, "0123456789abcdefghijklmnopqrstuvwxyzABCD") field(INPA, "1234567890123456789012345678901234567890123456789") }
record(printf, "PS0") { field(FMT, "[%.0ls]") field(INP0, "SRC40.DESC$ NPP") }
record(printf, "PS1") { field(FMT, "[%.1ls]") field(INP0, "SRC40.DESC$ NPP") }
record(printf, "PS3") { field(FMT, "[%.3ls]") field(INP0, "SRC40.DESC$ NPP") }
record(printf, "PS5") { field(FMT, "[%.5ls]") field(INP0, "SRC40.DESC$ NPP") }
record(printf, "PS39") { field(FMT, "[%.39ls]") field(INP0, "SRC40.DESC$ NPP") }
record(printf, "PS40") { field(FMT, "[%.40ls]") field(INP0, "SRC40.DESC$ NPP") }
record(printf, "PS45") { field(FMT, "[%.45ls]") field(INP0, "SRC40.DESC$ NPP") }
record(printf, "PSL") { field(FMT, "[%-8.2ls]") field(INP0, "SRC40.DESC$ NPP") }
record(printf, "PSA") { field(FMT, "[%.3ls]") field(INP0, "SRC40.INPA$ NPP") }
record(printf, "PSF") { field(FMT, "[%.3ls]") field(INP0, "SRC.DESC$[2:7] NPP") }
record(printf, "PSF1") { field(FMT, "[%.1ls]") field(INP0, "SRC.DESC$[2:7] NPP") }
record(printf, "PSK") { field(FMT, "[%.3ls]") field(INP0, {const:"hello_world"}) }
record(printf, "PSKN") { field(FMT, "[%.3ls]") field(INP0, {const:["hello_world"]}) }
record(printf, "PSKA") { field(FMT, "[%.3ls]") field(INP0, "[\"hello_world\"]") }
record(printf, "PSK0") { field(FMT, "[%ls]") field(INP0, {const:"hello_world"}) }
record(printf, "PSQ") { field(FMT, "[%.3ls]") field(INP0, "\"hello_world\"") }
record(longin, "N3") { field(VAL, "3") }
record(printf, "PST") { field(FMT, "[%.*ls]") field(INP0, "3") field(INP1, "SRC40.DESC$ NPP") }
record(printf, "PSTD") { field(FMT, "[%.*ls]") field(INP0, "N3 NPP") field(INP1, "SRC40.DESC$ NPP") }
record(printf, "PSTK") { field(FMT, "[%.*ls]") field(INP0, "3") field(INP1, {const:"hello_world"}) }
record(printf, "PST2") { field(FMT, "[%.*ls|%.*ls]") field(INP0, "2") field(INP1, "SRC40.DESC$ NPP") field(INP2, "5") field(INP3, "SRC40.DESC$ NPP") }
record(printf, "PSTN") { field(FMT, "[%.*ls]") field(INP0, "-2") field(INP1, "SRC40.DESC$ NPP") }
record(printf, "PSTW") { field(FMT, "[%*.*ls]") field(INP0, "8") field(INP1, "3") field(INP2, "SRC40.DESC$ NPP") }
record(printf, "PSTNW") { field(FMT, "[%*.*ls]") field(INP0, "-8") field(INP1, "2") field(INP2, "SRC40.DESC$ NPP") }
record(lsi, "LSK") { field(SIZV, "16") field(INP, {const:"0123456789abcdefghij"}) }
record(lsi, "LSA") { field(SIZV, "16") field(INP, "[\"0123456789abcdefghij\"]") }
record(lso, "LOK") { field(SIZV, "16") field(DOL, {const:"0123456789abcdefghij"}) }
record(lso, "LOA") { field(SIZV, "16") field(DOL, "[\"0123456789abcdefghij\"]") }
"#;

#[epics_macros_rs::epics_test]
async fn a_printf_ls_precision_reads_one_char_more_than_it_prints() {
    let db = IocBuilder::new()
        .db_string(PRINTF_DB, &std::collections::HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;
    let printf = [
        "PS0", "PS1", "PS3", "PS5", "PS39", "PS40", "PS45", "PSL", "PSA", "PSF", "PSF1", "PSK",
        "PSKN", "PSKA", "PSK0", "PSQ", "PST", "PSTD", "PSTK", "PST2", "PSTN", "PSTW", "PSTNW",
    ];
    for rec in printf {
        process(&db, rec).await;
    }
    let full = "[0123456789abcdefghijklmnopqrstuvwxyzABC";
    let want = [
        ("PS0", full, "41.0", "0.0"),
        ("PS1", "[0]", "4.0", "0.0"),
        ("PS3", "[012]", "6.0", "0.0"),
        ("PS5", "[01234]", "8.0", "0.0"),
        ("PS39", full, "41.0", "0.0"),
        ("PS40", full, "41.0", "0.0"),
        ("PS45", full, "41.0", "0.0"),
        ("PSL", "[01      ]", "11.0", "0.0"),
        ("PSA", "[123]", "6.0", "0.0"),
        ("PSF", "[cde]", "6.0", "0.0"),
        ("PSF1", "[c]", "4.0", "0.0"),
        ("PSK", "[hel]", "6.0", "0.0"),
        ("PSKN", "[hel]", "6.0", "0.0"),
        ("PSKA", "[hel]", "6.0", "0.0"),
        ("PSK0", "[hello_world]", "14.0", "0.0"),
        ("PSQ", "[LNK]", "6.0", "3.0"),
        // A `*` precision is read before the string, which asks for it.
        ("PST", "[012]", "6.0", "0.0"),
        ("PSTD", "[012]", "6.0", "0.0"),
        ("PSTK", "[hel]", "6.0", "0.0"),
        ("PST2", "[01|01234]", "11.0", "0.0"),
        ("PSTN", full, "41.0", "0.0"),
        // softIoc: `[     012` with LEN 41, its right-justify's `added -=
        // trunc` running past the text (`printfRecord.c:268-271`).
        ("PSTW", "[     012]", "11.0", "0.0"),
        ("PSTNW", "[01      ]", "11.0", "0.0"),
        // Loaded at init and never processed, so still UDF.
        ("LSK", "0123456789abcde", "16.0", "3.0"),
        ("LSA", "0123456789abcde", "16.0", "3.0"),
        ("LOK", "0123456789abcde", "16.0", "3.0"),
        ("LOA", "0123456789abcde", "16.0", "3.0"),
    ];
    let row = |rec: &str| {
        let get = |field: &str| render(db.get_pv(format!("{rec}.{field}")).unwrap());
        (rec.to_string(), get("VAL"), get("LEN"), get("SEVR"))
    };
    let got: Vec<_> = want.iter().map(|(rec, ..)| row(rec)).collect();
    let want: Vec<_> = want
        .iter()
        .map(|(rec, val, len, sevr)| {
            (
                rec.to_string(),
                val.to_string(),
                len.to_string(),
                sevr.to_string(),
            )
        })
        .collect();
    assert_eq!(got, want);
}

/// A CA read of a `$` string channel is `dbGet` at the request's type and
/// count, so its DBR_CHAR tail ends that read, filtered or not
/// (`camessage.c:510-514`, `dbAccess.c:1037-1044`); a monitor at autosize
/// counts `strlen + 1`. caget/camonitor against oracleStdIoc on `SRC`.
#[cfg(tokio_backend)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_ca_read_of_a_dollar_string_channel_ends_like_dbget() {
    use epics_base_rs::types::{DBR_CHAR, DBR_LONG};
    use epics_ca_rs::client::{CaChannel, CaClient, EnumReadback};
    use epics_ca_rs::protocol::{DBE_ALARM, DBE_LOG, DBE_VALUE};
    use epics_ca_rs::server::CaServer;

    /// The elements of a read, as caget prints them (DBR_CHAR signed).
    fn elements(v: &EpicsValue) -> Vec<i64> {
        match v {
            EpicsValue::CharArray(b) | EpicsValue::UCharArray(b) => {
                b.iter().map(|&c| i64::from(c as i8)).collect()
            }
            EpicsValue::Char(c) | EpicsValue::UChar(c) => vec![i64::from(*c as i8)],
            EpicsValue::LongArray(a) => a.iter().map(|&x| i64::from(x)).collect(),
            EpicsValue::DoubleArray(a) => a.iter().map(|&x| x as i64).collect(),
            v => vec![v.to_f64().unwrap_or_else(|| panic!("{v:?}")) as i64],
        }
    }

    let server = CaServer::builder()
        .port(0)
        .db_string(
            r#"record(calc, "SRC") { field(DESC, "abcdefgh") }"#,
            &Default::default(),
        )
        .unwrap()
        .build()
        .await
        .expect("build CA server");
    let port = server.udp_port();
    let _run = tokio::spawn(async move { server.run().await });
    // SAFETY: nextest runs each test in its own process, and this is set
    // before `CaClient::new` reads it.
    unsafe {
        std::env::set_var("EPICS_CA_ADDR_LIST", format!("127.0.0.1:{port}"));
        std::env::set_var("EPICS_CA_AUTO_ADDR_LIST", "NO");
        std::env::set_var("EPICS_CA_SERVER_PORT", port.to_string());
    }
    let client = CaClient::new().await.expect("client");
    let ts = r#"SRC.DESC${"ts":{"num":"ts","epoch":"unix"}}"#;
    let mut channels: Vec<(&str, CaChannel, u32)> = Vec::new();
    for pv in ["SRC.DESC$", "SRC.DESC$[2:5]", ts] {
        let ch = client.create_channel(pv);
        ch.wait_connected(fact_budget::FACT_BUDGET)
            .await
            .unwrap_or_else(|e| panic!("{pv} connects: {e:?}"));
        let count = ch.info().await.expect("info").element_count;
        channels.push((pv, ch, count));
    }
    let mut got = Vec::new();
    for n in [0u32, 1, 3] {
        for (pv, ch, count) in &channels {
            for (dbr, name) in [(DBR_LONG, "DBR_LONG"), (DBR_CHAR, "DBR_CHAR")] {
                // caget's `reqElems > nElems ? nElems : reqElems`, 0 the count.
                let snap = ch
                    .get_with_dbr_type(dbr, n.min(*count), fact_budget::FACT_BUDGET)
                    .await
                    .unwrap_or_else(|e| panic!("caget -d {name} -# {n} {pv}: {e:?}"));
                got.push((
                    format!("caget -d {name} -# {n} {pv}"),
                    elements(&snap.value),
                ));
            }
            // camonitor's count: 0 is autosize.
            let mask = DBE_VALUE | DBE_LOG | DBE_ALARM;
            let mut mon = match n {
                0 => ch.subscribe_with_mask_autosize(0.0, mask).await,
                n => {
                    ch.subscribe_with_mask_readback_count(
                        0.0,
                        mask,
                        EnumReadback::Native,
                        false,
                        Some(n.min(*count)),
                    )
                    .await
                }
            }
            .expect("subscribe");
            let event = tokio::time::timeout(fact_budget::FACT_BUDGET, mon.recv())
                .await
                .unwrap_or_else(|_| panic!("camonitor -# {n} {pv}: no event"))
                .expect("monitor open")
                .expect("event");
            got.push((format!("camonitor -# {n} {pv}"), elements(&event.value)));
        }
    }
    let mut padded = vec![97, 98, 99, 100, 101, 102, 103, 104];
    padded.resize(41, 0);
    let rows: [(&str, Vec<i64>); 27] = [
        ("caget -d DBR_LONG -# 0 SRC.DESC$", padded.clone()),
        ("caget -d DBR_CHAR -# 0 SRC.DESC$", padded),
        (
            "camonitor -# 0 SRC.DESC$",
            vec![97, 98, 99, 100, 101, 102, 103, 104, 0],
        ),
        (
            "caget -d DBR_LONG -# 0 SRC.DESC$[2:5]",
            vec![99, 100, 101, 102],
        ),
        (
            "caget -d DBR_CHAR -# 0 SRC.DESC$[2:5]",
            vec![99, 100, 101, 0],
        ),
        ("camonitor -# 0 SRC.DESC$[2:5]", vec![99, 100, 101, 0]),
        ("caget -d DBR_LONG -# 0 ts", vec![631152000, 0]),
        ("caget -d DBR_CHAR -# 0 ts", vec![-128, 0]),
        ("camonitor -# 0 ts", vec![631152000, 0]),
        ("caget -d DBR_LONG -# 1 SRC.DESC$", vec![97]),
        ("caget -d DBR_CHAR -# 1 SRC.DESC$", vec![0]),
        ("camonitor -# 1 SRC.DESC$", vec![0]),
        ("caget -d DBR_LONG -# 1 SRC.DESC$[2:5]", vec![99]),
        ("caget -d DBR_CHAR -# 1 SRC.DESC$[2:5]", vec![0]),
        ("camonitor -# 1 SRC.DESC$[2:5]", vec![0]),
        ("caget -d DBR_LONG -# 1 ts", vec![631152000]),
        ("caget -d DBR_CHAR -# 1 ts", vec![0]),
        ("camonitor -# 1 ts", vec![631152000]),
        ("caget -d DBR_LONG -# 3 SRC.DESC$", vec![97, 98, 99]),
        ("caget -d DBR_CHAR -# 3 SRC.DESC$", vec![97, 98, 0]),
        ("camonitor -# 3 SRC.DESC$", vec![97, 98, 0]),
        ("caget -d DBR_LONG -# 3 SRC.DESC$[2:5]", vec![99, 100, 101]),
        ("caget -d DBR_CHAR -# 3 SRC.DESC$[2:5]", vec![99, 100, 0]),
        ("camonitor -# 3 SRC.DESC$[2:5]", vec![99, 100, 0]),
        ("caget -d DBR_LONG -# 3 ts", vec![631152000, 0]),
        ("caget -d DBR_CHAR -# 3 ts", vec![-128, 0]),
        ("camonitor -# 3 ts", vec![631152000, 0]),
    ];
    let want: Vec<_> = rows
        .into_iter()
        .map(|(row, v)| (row.replace(" ts", &format!(" {ts}")), v))
        .collect();
    assert_eq!(got, want);
}
