//! C `dbGetLink(plink, dbrType, ...)` converts at the reader's `dbrType`
//! inside the read, so a source that request cannot convert fails the read
//! and `setLinkAlarm` raises LINK/INVALID with AMSG `field <link>`
//! (`dbLink.c:335-339`). The source here is a stringin holding `"abc"`, which
//! no numeric request converts; `"5"` is the live boundary.

use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::alarm_status;
use epics_base_rs::server::record::AlarmSeverity;
use epics_base_rs::server::records::busy::BusyRecord;
use epics_base_rs::server::records::sseq::SseqRecord;
use epics_base_rs::server::records::swait::SwaitRecord;
use epics_base_rs::types::EpicsValue;

const DB: &str = r#"
record(stringin, "ABC") { field(VAL, "abc") }
record(stringin, "FIVE") { field(VAL, "5") }

record(mbbiDirect, "MBID:ABC") { field(SIMM, "YES") field(SIOL, "ABC NPP") field(VAL, "1") }
record(mbbiDirect, "MBID:FIVE") { field(SIMM, "YES") field(SIOL, "FIVE NPP") }

record(ai, "AI:ABC") { field(SIMM, "YES") field(SIOL, "ABC NPP") }
record(bi, "BI:ABC") { field(SIMM, "YES") field(SIOL, "ABC NPP") }
record(mbbi, "MBI:ABC") { field(SIMM, "YES") field(SIOL, "ABC NPP") }
record(longin, "LI:ABC") { field(SIMM, "YES") field(SIOL, "ABC NPP") field(VAL, "2") }
record(int64in, "I64I:ABC") { field(SIMM, "YES") field(SIOL, "ABC NPP") }
record(longin, "LI:FIVE") { field(SIMM, "YES") field(SIOL, "FIVE NPP") }

record(swait, "SW:ABC") { field(SIMM, "YES") field(SIOL, "ABC NPP") field(VAL, "2") field(CALC, "A") }
record(swait, "SW:FIVE") { field(SIMM, "YES") field(SIOL, "FIVE NPP") field(CALC, "A") }

record(waveform, "SRC") { field(FTVL, "DOUBLE") field(NELM, "4") field(INP, "[1.5, 2.5, 3.5]") }
record(waveform, "WF:ABC") { field(FTVL, "LONG") field(NELM, "4") field(INP, "ABC NPP") }
record(aai, "AAI:ABC") { field(SIMM, "YES") field(SIOL, "ABC NPP") field(FTVL, "DOUBLE") field(NELM, "4") }
record(subArray, "SA:ABC") { field(FTVL, "DOUBLE") field(MALM, "4") field(NELM, "2") field(INP, "ABC NPP") }
record(waveform, "WF:STR") { field(FTVL, "STRING") field(NELM, "3") field(INP, "SRC NPP") }

record(ao, "F37") { field(VAL, "3.7") }
record(ao, "BIG") { field(VAL, "70000") }
record(ao, "NEG") { field(VAL, "-1") }

record(ao, "AO:ABC") { field(OMSL, "closed_loop") field(DOL, "ABC NPP") field(VAL, "2") }
record(ao, "AO:FIVE") { field(OMSL, "closed_loop") field(DOL, "FIVE NPP") }

record(longout, "LO:ABC") { field(OMSL, "closed_loop") field(DOL, "ABC NPP") field(VAL, "2") }
record(longout, "LO:F37") { field(OMSL, "closed_loop") field(DOL, "F37 NPP") }

record(int64out, "I64:ABC") { field(OMSL, "closed_loop") field(DOL, "ABC NPP") field(VAL, "2") }
record(int64out, "I64:F37") { field(OMSL, "closed_loop") field(DOL, "F37 NPP") }

record(bo, "BO:ABC") { field(OMSL, "closed_loop") field(DOL, "ABC NPP") field(VAL, "1") }
record(bo, "BO:F37") { field(OMSL, "closed_loop") field(DOL, "F37 NPP") }

record(mbbo, "MB:ABC") { field(OMSL, "closed_loop") field(DOL, "ABC NPP") field(VAL, "1") field(ZRVL, "10") field(ONVL, "11") }
record(mbbo, "MB:BIG") { field(OMSL, "closed_loop") field(DOL, "BIG NPP") }

record(mbboDirect, "MBD:ABC") { field(OMSL, "closed_loop") field(DOL, "ABC NPP") field(VAL, "1") }
record(mbboDirect, "MBD:NEG") { field(OMSL, "closed_loop") field(DOL, "NEG NPP") }

record(busy, "BUSY:ABC") { field(OMSL, "closed_loop") field(DOL, "ABC NPP") }

record(dfanout, "DF:ABC") { field(OMSL, "closed_loop") field(DOL, "ABC NPP") field(VAL, "2") field(OUTA, "DF:O") }
record(ao, "DF:O") { }
record(dfanout, "DFS:ABC") { field(SELM, "Specified") field(SELL, "ABC NPP") field(SELN, "1") field(VAL, "3") field(OUTA, "DFS:O") }
record(ao, "DFS:O") { }
record(dfanout, "DFS:F37") { field(SELM, "Specified") field(SELL, "F37 NPP") field(SELN, "1") }

record(fanout, "FO:ABC") { field(SELM, "Specified") field(SELL, "ABC NPP") field(SELN, "1") field(LNK1, "FO:T") }
record(calc, "FO:T") { field(CALC, "VAL+1") }
record(fanout, "FO:F37") { field(SELM, "Specified") field(SELL, "F37 NPP") field(SELN, "1") }

record(seq, "SEQ:ABC") { field(DOL1, "ABC NPP") field(DO1, "7") field(LNK1, "SEQ:O PP") }
record(ao, "SEQ:O") { }
record(seq, "SEQS:ABC") { field(SELM, "Specified") field(SELL, "ABC NPP") field(SELN, "1") field(DO1, "5") field(LNK1, "SEQS:O PP") }
record(ao, "SEQS:O") { }
record(seq, "SEQ:F37") { field(DOL1, "F37 NPP") }

record(sel, "SEL:ABC") { field(SELM, "Specified") field(NVL, "ABC NPP") field(SELN, "1") field(INPA, "1") field(INPB, "2") }
record(sseq, "SS:ABC") { field(SELM, "Specified") field(SELL, "ABC NPP") field(SELN, "1") field(DO1, "4") field(LNK1, "SS:O PP") field(DO2, "9") field(LNK2, "SS:O2 PP") }
record(ao, "SS:O") { }
record(ao, "SS:O2") { }
record(sseq, "SS:F37") { field(SELM, "Specified") field(SELL, "F37 NPP") }
record(sseq, "SS:FIVE") { field(SELM, "Specified") field(SELL, "FIVE NPP") }
record(sseq, "SS:ARR") { field(SELM, "Specified") field(SELL, "SRC NPP") }
record(sseq, "SS:BIG") { field(SELM, "Mask") field(SELL, "BIG NPP") }
record(sseq, "SS:NEG") { field(SELM, "Mask") field(SELL, "NEG NPP") }
record(sel, "SEL:F37") { field(SELM, "Specified") field(NVL, "F37 NPP") field(INPD, "4") }

record(stringin, "S1E3") { field(VAL, "1e3") }
record(busy, "BUSY:SIML:ABC") { field(SIML, "ABC NPP") field(SIOL, "0") field(VAL, "0") }
record(busy, "BUSY:SIML:1E3") { field(SIML, "S1E3 NPP") field(SIOL, "0") field(VAL, "0") }
record(busy, "BUSY:SIML:BIG") { field(SIML, "BIG NPP") field(SIOL, "0") field(VAL, "0") }
record(swait, "SW:SIML:FIVE") { field(SIML, "FIVE NPP") field(SIOL, "0") }
record(swait, "SW:SIML:BIG") { field(SIML, "BIG NPP") field(SIOL, "0") }
record(swait, "SW:SIML:NEG") { field(SIML, "NEG NPP") field(SIOL, "0") }

record(stringin, "EMPTY") { field(VAL, "") }
record(compress, "CMP:ABC") { field(INP, "ABC NPP") field(NSAM, "2") field(ALG, "Circular Buffer") }
record(compress, "CMP:EMPTY") { field(INP, "EMPTY NPP") field(NSAM, "2") field(ALG, "Circular Buffer") }
record(aao, "AAO:EMPTY") { field(OMSL, "closed_loop") field(DOL, "EMPTY NPP") field(FTVL, "DOUBLE") field(NELM, "4") }
record(aao, "AAO:F37") { field(OMSL, "closed_loop") field(DOL, "F37 NPP") field(FTVL, "LONG") field(NELM, "4") }
"#;

async fn build() -> std::sync::Arc<epics_base_rs::server::database::PvDatabase> {
    IocBuilder::new()
        .register_record_type("busy", || Box::new(BusyRecord::default()))
        .register_record_type("swait", || Box::new(SwaitRecord::default()))
        .register_record_type("sseq", || Box::new(SseqRecord::default()))
        .db_string(DB, &std::collections::HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0
}

/// The cycle's status is C's `process()` return, a failed read's included;
/// what the record published is what these tests read.
async fn process(db: &epics_base_rs::server::database::PvDatabase, rec: &str) {
    let mut visited = epics_base_rs::server::database::ProcStack::new();
    let _ = db.process_record_with_links(rec, &mut visited).await;
}

/// seq hands its groups to the callback task and returns with `pact` set
/// (`seqRecord.c:143`), so its reads and alarm land after `process` returns.
async fn settle(db: &epics_base_rs::server::database::PvDatabase, rec: &str) {
    for _ in 0..400 {
        if !db.get_record(rec).unwrap().read().is_processing() {
            return;
        }
        epics_base_rs::runtime::task::sleep(std::time::Duration::from_millis(5)).await;
    }
    panic!("{rec} never left PACT");
}

fn val(db: &epics_base_rs::server::database::PvDatabase, rec: &str) -> Option<EpicsValue> {
    db.get_record(rec).unwrap().read().record.get_field("VAL")
}

fn field(
    db: &epics_base_rs::server::database::PvDatabase,
    rec: &str,
    f: &str,
) -> Option<EpicsValue> {
    db.get_record(rec).unwrap().read().record.get_field(f)
}

fn alarm(
    db: &epics_base_rs::server::database::PvDatabase,
    rec: &str,
) -> (AlarmSeverity, u16, String) {
    let r = db.get_record(rec).unwrap();
    let c = &r.read().common;
    (c.sevr, c.stat, c.amsg.to_string())
}

fn link_alarm(field: &str) -> (AlarmSeverity, u16, String) {
    (
        AlarmSeverity::Invalid,
        alarm_status::LINK_ALARM,
        format!("field {field}"),
    )
}

/// mbbiDirect reads SIOL at `DBR_ULONG` (`mbbiDirectRecord.c` `readValue`):
/// the request is declared, and its conversion failure is the read's.
#[epics_macros_rs::epics_test]
async fn mbbi_direct_siol_unconvertible_is_a_failed_read() {
    let db = build().await;
    process(&db, "MBID:ABC").await;
    assert_eq!(alarm(&db, "MBID:ABC"), link_alarm("SIOL"));
    assert_eq!(val(&db, "MBID:ABC"), Some(EpicsValue::Long(1)));

    process(&db, "MBID:FIVE").await;
    assert_eq!(val(&db, "MBID:FIVE"), Some(EpicsValue::Long(5)));
    assert_eq!(alarm(&db, "MBID:FIVE").0, AlarmSeverity::NoAlarm);
}

/// The base input records read SIOL into SVAL at SVAL's own type
/// (`longinRecord.c:416` `dbGetLink(&prec->siol, DBR_LONG, &prec->sval)`),
/// so a value SVAL refuses is that read's failed conversion.
#[epics_macros_rs::epics_test]
async fn siol_into_sval_unconvertible_is_a_failed_read() {
    let db = build().await;
    for rec in ["AI:ABC", "BI:ABC", "MBI:ABC", "LI:ABC", "I64I:ABC"] {
        process(&db, rec).await;
        assert_eq!(alarm(&db, rec), link_alarm("SIOL"), "{rec}");
    }
    assert_eq!(val(&db, "LI:ABC"), Some(EpicsValue::Long(2)));

    process(&db, "LI:FIVE").await;
    assert_eq!(val(&db, "LI:FIVE"), Some(EpicsValue::Long(5)));
    assert_eq!(alarm(&db, "LI:FIVE").0, AlarmSeverity::NoAlarm);
}

/// swait reads SIOL at `DBR_DOUBLE` (`swaitRecord.c:416`).
#[epics_macros_rs::epics_test]
async fn swait_siol_unconvertible_is_a_failed_read() {
    let db = build().await;
    process(&db, "SW:ABC").await;
    assert_eq!(alarm(&db, "SW:ABC"), link_alarm("SIOL"));
    assert_eq!(val(&db, "SW:ABC"), Some(EpicsValue::Double(2.0)));

    process(&db, "SW:FIVE").await;
    assert_eq!(val(&db, "SW:FIVE"), Some(EpicsValue::Double(5.0)));
    assert_eq!(alarm(&db, "SW:FIVE").0, AlarmSeverity::NoAlarm);
}

/// The array soft dsets read INP, and SIOL under SIMM, at `(FTVL, nRequest)`
/// (`devWfSoft.c:63`, `devAaiSoft.c:73`, `devSASoft.c:84`): a STRING FTVL
/// renders a DOUBLE source at its PREC, as `dbGetLink` does.
#[epics_macros_rs::epics_test]
async fn array_inp_and_siol_read_at_ftvl() {
    let db = build().await;
    for (rec, link) in [("WF:ABC", "INP"), ("AAI:ABC", "SIOL"), ("SA:ABC", "INP")] {
        process(&db, rec).await;
        assert_eq!(alarm(&db, rec), link_alarm(link), "{rec}");
    }

    process(&db, "WF:STR").await;
    assert_eq!(
        val(&db, "WF:STR"),
        Some(EpicsValue::StringArray(
            vec!["2".into(), "3".into(), "4".into()].into()
        ))
    );
}

/// ao reads DOL at `DBR_DOUBLE` (`aoRecord.c:444`); the failure arm reverts
/// a client's put to PVAL (`:441`) and skips `convert`.
#[epics_macros_rs::epics_test]
async fn ao_dol_unconvertible_is_a_failed_read() {
    let db = build().await;
    db.put_pv("AO:ABC", EpicsValue::Double(7.0)).await.unwrap();
    process(&db, "AO:ABC").await;
    assert_eq!(alarm(&db, "AO:ABC"), link_alarm("DOL"));
    assert_eq!(val(&db, "AO:ABC"), Some(EpicsValue::Double(2.0)));

    process(&db, "AO:FIVE").await;
    assert_eq!(val(&db, "AO:FIVE"), Some(EpicsValue::Double(5.0)));
    assert_eq!(alarm(&db, "AO:FIVE").0, AlarmSeverity::NoAlarm);
}

/// longout reads DOL at `DBR_LONG` (`longoutRecord.c:148`): `3.7` truncates.
#[epics_macros_rs::epics_test]
async fn longout_dol_unconvertible_is_a_failed_read() {
    let db = build().await;
    process(&db, "LO:ABC").await;
    assert_eq!(alarm(&db, "LO:ABC"), link_alarm("DOL"));
    assert_eq!(val(&db, "LO:ABC"), Some(EpicsValue::Long(2)));

    process(&db, "LO:F37").await;
    assert_eq!(val(&db, "LO:F37"), Some(EpicsValue::Long(3)));
}

/// int64out reads DOL at `DBR_INT64` (`int64outRecord.c:138`).
#[epics_macros_rs::epics_test]
async fn int64out_dol_unconvertible_is_a_failed_read() {
    let db = build().await;
    process(&db, "I64:ABC").await;
    assert_eq!(alarm(&db, "I64:ABC"), link_alarm("DOL"));
    assert_eq!(val(&db, "I64:ABC"), Some(EpicsValue::Int64(2)));

    process(&db, "I64:F37").await;
    assert_eq!(val(&db, "I64:F37"), Some(EpicsValue::Int64(3)));
}

/// bo reads DOL at `DBR_USHORT` (`boRecord.c:197`): `3.7` is 3, stored as is.
#[epics_macros_rs::epics_test]
async fn bo_dol_unconvertible_is_a_failed_read() {
    let db = build().await;
    process(&db, "BO:ABC").await;
    assert_eq!(alarm(&db, "BO:ABC"), link_alarm("DOL"));
    assert_eq!(val(&db, "BO:ABC"), Some(EpicsValue::Enum(1)));

    process(&db, "BO:F37").await;
    assert_eq!(val(&db, "BO:F37"), Some(EpicsValue::Enum(3)));
}

/// mbbo reads DOL at `DBR_USHORT` (`mbboRecord.c:204`): 70000 wraps to 4464,
/// and the failure arm's `goto CONTINUE` keeps the init RVAL.
#[epics_macros_rs::epics_test]
async fn mbbo_dol_unconvertible_is_a_failed_read() {
    let db = build().await;
    process(&db, "MB:ABC").await;
    assert_eq!(alarm(&db, "MB:ABC"), link_alarm("DOL"));
    assert_eq!(val(&db, "MB:ABC"), Some(EpicsValue::Enum(1)));
    assert_eq!(field(&db, "MB:ABC", "RVAL"), Some(EpicsValue::ULong(11)));

    process(&db, "MB:BIG").await;
    assert_eq!(val(&db, "MB:BIG").and_then(|v| v.to_f64()), Some(4464.0));
}

/// mbboDirect reads DOL at `DBR_ULONG` (`mbboDirectRecord.c:184`): -1 lands
/// as all bits.
#[epics_macros_rs::epics_test]
async fn mbbo_direct_dol_unconvertible_is_a_failed_read() {
    let db = build().await;
    process(&db, "MBD:ABC").await;
    assert_eq!(alarm(&db, "MBD:ABC"), link_alarm("DOL"));
    assert_eq!(val(&db, "MBD:ABC"), Some(EpicsValue::Long(1)));

    process(&db, "MBD:NEG").await;
    assert_eq!(val(&db, "MBD:NEG"), Some(EpicsValue::Long(-1)));
    assert_eq!(
        field(&db, "MBD:NEG", "RVAL"),
        Some(EpicsValue::ULong(u32::MAX))
    );
}

/// busy reads DOL at `DBR_USHORT` (`busyRecord.c:200`).
#[epics_macros_rs::epics_test]
async fn busy_dol_unconvertible_is_a_failed_read() {
    let db = build().await;
    process(&db, "BUSY:ABC").await;
    assert_eq!(alarm(&db, "BUSY:ABC"), link_alarm("DOL"));
    assert_eq!(val(&db, "BUSY:ABC"), Some(EpicsValue::Enum(0)));
}

/// dfanout reads DOL at `DBR_DOUBLE` (`dfanoutRecord.c:119`) and SELL at
/// `DBR_USHORT` (`:126`): a failed SELL read leaves SELN, so OUTA is still
/// driven.
#[epics_macros_rs::epics_test]
async fn dfanout_dol_and_sell_unconvertible_are_failed_reads() {
    let db = build().await;
    process(&db, "DF:ABC").await;
    assert_eq!(alarm(&db, "DF:ABC"), link_alarm("DOL"));
    assert_eq!(val(&db, "DF:O"), Some(EpicsValue::Double(2.0)));

    process(&db, "DFS:ABC").await;
    assert_eq!(alarm(&db, "DFS:ABC"), link_alarm("SELL"));
    assert_eq!(field(&db, "DFS:ABC", "SELN"), Some(EpicsValue::UShort(1)));
    assert_eq!(val(&db, "DFS:O"), Some(EpicsValue::Double(3.0)));

    process(&db, "DFS:F37").await;
    assert_eq!(field(&db, "DFS:F37", "SELN"), Some(EpicsValue::UShort(3)));
}

/// fanout reads SELL at `DBR_USHORT` (`fanoutRecord.c:103`): a failed read
/// leaves SELN, so LNK1 still fires.
#[epics_macros_rs::epics_test]
async fn fanout_sell_unconvertible_is_a_failed_read() {
    let db = build().await;
    process(&db, "FO:ABC").await;
    assert_eq!(alarm(&db, "FO:ABC"), link_alarm("SELL"));
    assert_eq!(field(&db, "FO:ABC", "SELN"), Some(EpicsValue::UShort(1)));
    assert_eq!(val(&db, "FO:T"), Some(EpicsValue::Double(1.0)));

    process(&db, "FO:F37").await;
    assert_eq!(field(&db, "FO:F37", "SELN"), Some(EpicsValue::UShort(3)));
}

/// seq reads DOLn at `DBR_DOUBLE` (`seqRecord.c:259`) and SELL at
/// `DBR_USHORT` (`:152`): a failed DOLn read keeps DOn, a failed SELL read
/// keeps SELN, and LNK1 is driven either way.
#[epics_macros_rs::epics_test]
async fn seq_doln_and_sell_unconvertible_are_failed_reads() {
    let db = build().await;
    process(&db, "SEQ:ABC").await;
    settle(&db, "SEQ:ABC").await;
    assert_eq!(alarm(&db, "SEQ:ABC"), link_alarm("DOL1"));
    assert_eq!(field(&db, "SEQ:ABC", "DO1"), Some(EpicsValue::Double(7.0)));
    assert_eq!(val(&db, "SEQ:O"), Some(EpicsValue::Double(7.0)));

    process(&db, "SEQS:ABC").await;
    settle(&db, "SEQS:ABC").await;
    assert_eq!(alarm(&db, "SEQS:ABC"), link_alarm("SELL"));
    assert_eq!(field(&db, "SEQS:ABC", "SELN"), Some(EpicsValue::UShort(1)));
    assert_eq!(val(&db, "SEQS:O"), Some(EpicsValue::Double(5.0)));

    process(&db, "SEQ:F37").await;
    settle(&db, "SEQ:F37").await;
    assert_eq!(field(&db, "SEQ:F37", "DO1"), Some(EpicsValue::Double(3.7)));
}

/// sel reads NVL at `DBR_USHORT` (`selRecord.c:423`): a failed read is
/// `dbGetLink`'s, with its LINK alarm, and leaves SELN and VAL.
#[epics_macros_rs::epics_test]
async fn sel_nvl_unconvertible_is_a_failed_read() {
    let db = build().await;
    process(&db, "SEL:ABC").await;
    assert_eq!(alarm(&db, "SEL:ABC"), link_alarm("NVL"));
    assert_eq!(field(&db, "SEL:ABC", "SELN"), Some(EpicsValue::UShort(1)));
    assert_eq!(val(&db, "SEL:ABC"), Some(EpicsValue::Double(0.0)));

    process(&db, "SEL:F37").await;
    assert_eq!(field(&db, "SEL:F37", "SELN"), Some(EpicsValue::UShort(3)));
    assert_eq!(val(&db, "SEL:F37"), Some(EpicsValue::Double(4.0)));
}

/// sseq reads SELL at `DBR_USHORT` (`sseqRecord.c:316`) and ignores the
/// status, so a failed read alarms LINK and runs the step SELN still names.
/// A regression guard: sseq's `ReadDbLink` stores through SELN's put rule,
/// which already agrees with C's GET table on every source here.
#[epics_macros_rs::epics_test]
async fn sseq_sell_unconvertible_is_a_failed_read() {
    let db = build().await;
    process(&db, "SS:ABC").await;
    settle(&db, "SS:ABC").await;
    assert_eq!(alarm(&db, "SS:ABC"), link_alarm("SELL"));
    assert_eq!(field(&db, "SS:ABC", "SELN"), Some(EpicsValue::UShort(1)));
    assert_eq!(val(&db, "SS:O"), Some(EpicsValue::Double(4.0)));
    assert_eq!(val(&db, "SS:O2"), Some(EpicsValue::Double(0.0)));

    process(&db, "SS:F37").await;
    settle(&db, "SS:F37").await;
    assert_eq!(field(&db, "SS:F37", "SELN"), Some(EpicsValue::UShort(3)));

    process(&db, "SS:FIVE").await;
    settle(&db, "SS:FIVE").await;
    assert_eq!(field(&db, "SS:FIVE", "SELN"), Some(EpicsValue::UShort(5)));
    assert_eq!(alarm(&db, "SS:FIVE").0, AlarmSeverity::NoAlarm);

    // An array source contributes element 0, as C asks for one element.
    process(&db, "SS:ARR").await;
    settle(&db, "SS:ARR").await;
    assert_eq!(field(&db, "SS:ARR", "SELN"), Some(EpicsValue::UShort(1)));
    assert_eq!(alarm(&db, "SS:ARR").0, AlarmSeverity::NoAlarm);

    process(&db, "SS:BIG").await;
    settle(&db, "SS:BIG").await;
    assert_eq!(field(&db, "SS:BIG", "SELN"), Some(EpicsValue::UShort(4464)));
    process(&db, "SS:NEG").await;
    settle(&db, "SS:NEG").await;
    assert_eq!(
        field(&db, "SS:NEG", "SELN"),
        Some(EpicsValue::UShort(65535))
    );
}

fn simm(db: &epics_base_rs::server::database::PvDatabase, rec: &str) -> Option<u16> {
    db.get_record(rec)
        .unwrap()
        .read()
        .record
        .simm_value()
        .map(|v| v as u16)
}

/// busy reads SIML itself, at `DBR_USHORT` (`busyRecord.c:399`): `"1e3"` is
/// 1, `70000` wraps to 4464, which no menu choice names.
#[epics_macros_rs::epics_test]
async fn busy_siml_is_read_at_dbr_ushort() {
    let db = build().await;
    process(&db, "BUSY:SIML:ABC").await;
    assert_eq!(
        (simm(&db, "BUSY:SIML:ABC"), alarm(&db, "BUSY:SIML:ABC")),
        (Some(0), link_alarm("SIML"))
    );
    process(&db, "BUSY:SIML:1E3").await;
    assert_eq!(
        (simm(&db, "BUSY:SIML:1E3"), alarm(&db, "BUSY:SIML:1E3").0),
        (Some(1), AlarmSeverity::NoAlarm)
    );
    process(&db, "BUSY:SIML:BIG").await;
    assert_eq!(
        (simm(&db, "BUSY:SIML:BIG"), alarm(&db, "BUSY:SIML:BIG")),
        (
            Some(4464),
            (
                AlarmSeverity::Invalid,
                alarm_status::SOFT_ALARM,
                String::new()
            )
        )
    );
}

/// swait reads SIML itself, at `DBR_ENUM` (`swaitRecord.c:402`). A string
/// source's `cvt_st_e` asks the source's `put_enum_str`, which stringin
/// has none of, so even `"5"` fails; a number converts and swait keeps
/// whatever it gets, with no menu check.
#[epics_macros_rs::epics_test]
async fn swait_siml_is_read_at_dbr_enum() {
    let db = build().await;
    process(&db, "SW:SIML:FIVE").await;
    assert_eq!(
        (simm(&db, "SW:SIML:FIVE"), alarm(&db, "SW:SIML:FIVE")),
        (Some(0), link_alarm("SIML"))
    );
    for (rec, simm_is) in [("SW:SIML:BIG", 4464), ("SW:SIML:NEG", 65535)] {
        process(&db, rec).await;
        assert_eq!(
            (simm(&db, rec), alarm(&db, rec).0),
            (Some(simm_is), AlarmSeverity::NoAlarm),
            "{rec}"
        );
    }
}

/// compress reads INP at `DBF_DOUBLE` (`compressRecord.c:342`): `"abc"`
/// fails the read with `setLinkAlarm`'s AMSG, and `""` is a sample of 0.
#[epics_macros_rs::epics_test]
async fn compress_inp_is_read_at_dbr_double() {
    let db = build().await;
    process(&db, "CMP:ABC").await;
    assert_eq!(alarm(&db, "CMP:ABC"), link_alarm("INP"));
    assert_eq!(field(&db, "CMP:ABC", "NUSE"), Some(EpicsValue::ULong(0)));

    process(&db, "CMP:EMPTY").await;
    assert_eq!(alarm(&db, "CMP:EMPTY").0, AlarmSeverity::NoAlarm);
    assert_eq!(field(&db, "CMP:EMPTY", "NUSE"), Some(EpicsValue::ULong(1)));
}

/// aao reads DOL at `(FTVL, NELM)` (`aaoRecord.c:366`): `""` is one 0, and
/// a DOUBLE source into FTVL LONG is truncated, not refused.
#[epics_macros_rs::epics_test]
async fn aao_dol_is_read_at_ftvl() {
    let db = build().await;
    process(&db, "AAO:EMPTY").await;
    assert_eq!(alarm(&db, "AAO:EMPTY").0, AlarmSeverity::NoAlarm);
    assert_eq!(field(&db, "AAO:EMPTY", "NORD"), Some(EpicsValue::ULong(1)));
    assert_eq!(
        val(&db, "AAO:EMPTY").and_then(|v| v.first_element()),
        Some(EpicsValue::Double(0.0))
    );

    process(&db, "AAO:F37").await;
    assert_eq!(field(&db, "AAO:F37", "NORD"), Some(EpicsValue::ULong(1)));
    assert_eq!(
        val(&db, "AAO:F37").and_then(|v| v.first_element()),
        Some(EpicsValue::Long(3))
    );
}

/// A `DBF_STRING` array read at FTVL ENUM takes `getStringUshort`
/// (`dbConvert.c:1710-1712`): each element parses as `epicsParseUInt16`,
/// and one that does not parse fails the whole read.
#[epics_macros_rs::epics_test]
async fn string_array_into_ftvl_enum_parses_as_ushort() {
    const SOURCES: [(&str, &str, Option<[u16; 2]>); 7] = [
        ("num", r#"["5","6"]"#, Some([5, 6])),
        ("abc", r#"["abc","1"]"#, None),
        ("emp", r#"["","7"]"#, Some([0, 7])),
        ("e3", r#"["1e3","2"]"#, Some([1, 2])),
        ("big", r#"["70000","1"]"#, None),
        ("neg", r#"["-1","1"]"#, Some([65535, 1])),
        ("hex", r#"["0x10","2"]"#, Some([16, 2])),
    ];
    // (prefix, link field, record body)
    const READERS: [(&str, &str, &str); 5] = [
        ("WF", "INP", r#"waveform, "WF:{k}") { field(INP, "W:{k}")"#),
        ("AI", "INP", r#"aai, "AI:{k}") { field(INP, "W:{k}")"#),
        (
            "SA",
            "INP",
            r#"subArray, "SA:{k}") { field(MALM, "2") field(INP, "W:{k}")"#,
        ),
        (
            "WS",
            "SIOL",
            r#"waveform, "WS:{k}") { field(SIMM, "YES") field(SIOL, "W:{k}")"#,
        ),
        (
            "AS",
            "SIOL",
            r#"aai, "AS:{k}") { field(SIMM, "YES") field(SIOL, "W:{k}")"#,
        ),
    ];
    let mut db_text = String::new();
    for (k, init, _) in SOURCES {
        db_text += &format!(
            "record(waveform, \"W:{k}\") {{ field(FTVL, \"STRING\") field(NELM, \"2\") field(INP, {init}) }}\n"
        );
        for (_, _, body) in READERS {
            db_text += &format!(
                "record({} field(FTVL, \"ENUM\") field(NELM, \"2\") }}\n",
                body.replace("{k}", k)
            );
        }
    }
    let db = IocBuilder::new()
        .db_string(&db_text, &std::collections::HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;
    let mut wrong = Vec::new();
    for (k, _, want) in SOURCES {
        for (p, link, _) in READERS {
            let rec = format!("{p}:{k}");
            process(&db, &rec).await;
            let got_alarm = alarm(&db, &rec);
            match want {
                None if got_alarm != link_alarm(link) => {
                    wrong.push(format!("{rec}: {got_alarm:?}, C LINK \"field {link}\""))
                }
                Some(want) => {
                    let got = match val(&db, &rec) {
                        Some(EpicsValue::EnumArray(a)) => Some(a.to_vec()),
                        other => panic!("{rec}: VAL {other:?}"),
                    };
                    if got.as_deref() != Some(&want[..]) || got_alarm.0 != AlarmSeverity::NoAlarm {
                        wrong.push(format!("{rec}: {got:?} {got_alarm:?}, C {want:?}"));
                    }
                }
                None => {}
            }
        }
    }
    assert!(wrong.is_empty(), "{wrong:#?}");
}
