//! R16-77: a CONSTANT link delivers NOTHING at process time; its value reaches
//! the record ONCE, at init, through the init-seed owner.
//!
//! C: `dbConstGetValue` (`dbConstLink.c:219-225`) sets `*pnRequest = 0` and
//! returns SUCCESS — so every `dbGetLink` on a constant link (INPA..L, NVL,
//! SELL, DOLn, SUBL, SDIS, TSEL, SIML) delivers nothing on every cycle. The
//! value is loaded once by each record's `recGblInitConstantLink` table
//! (`calcRecord.c:103`, `selRecord.c:99,105`, `seqRecord.c:121,125`,
//! `fanoutRecord.c:88`, `dfanoutRecord.c:102`, `aSubRecord.c:126`).
//!
//! softIoc (EPICS 7, linux-x86_64), `record(calc,"S1"){field(INPA,"5")
//! field(CALC,"A+1")}`:
//!
//! ```text
//! iocInit           -> A = 5          (seeded at init)
//! dbpf S1.A 99      -> A = 99
//! dbpf S1.PROC 1    -> A = 99, VAL = 100   (the constant did NOT come back)
//! ```
//!
//! Pre-fix the port re-applied the constant on every process, so the caput was
//! destroyed on the next scan and A read 0 before the first process.

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::records::calc::CalcRecord;
use epics_base_rs::server::records::printf::PrintfRecord;
use epics_base_rs::server::records::sel::SelRecord;
use epics_base_rs::server::records::seq::SeqRecord;
use epics_base_rs::types::EpicsValue;
use std::collections::HashSet;

async fn field(db: &PvDatabase, rec: &str, f: &str) -> EpicsValue {
    let r = db.get_record(rec).unwrap();
    let inst = r.read();
    inst.record.get_field(f).unwrap()
}

async fn process(db: &PvDatabase, rec: &str) {
    let mut visited = HashSet::new();
    db.process_record_with_links(rec, &mut visited, 0)
        .await
        .unwrap();
}

/// The whole rule on one record: seeded at init, never re-delivered, and a
/// client's put stands.
#[epics_macros_rs::epics_test]
async fn constant_input_is_seeded_at_init_and_never_re_applied() {
    let db = PvDatabase::new();
    let mut calc = CalcRecord::new("A+B");
    calc.inpa = "5".to_string();
    calc.inpb = "3".to_string();
    db.add_record("C1", Box::new(calc)).await.unwrap();

    // Seeded at init — BEFORE the first process (C reads A=5 right after
    // iocInit; the port used to read 0 here).
    assert_eq!(field(&db, "C1", "A").await, EpicsValue::Double(5.0));
    assert_eq!(field(&db, "C1", "B").await, EpicsValue::Double(3.0));

    process(&db, "C1").await;
    assert_eq!(field(&db, "C1", "VAL").await, EpicsValue::Double(8.0));

    // A client put to the value field survives the next process: the constant
    // link delivers nothing.
    db.put_pv("C1.A", EpicsValue::Double(99.0)).await.unwrap();
    process(&db, "C1").await;
    assert_eq!(
        field(&db, "C1", "A").await,
        EpicsValue::Double(99.0),
        "a constant INPA must not clobber the client's caput on the next process"
    );
    assert_eq!(
        field(&db, "C1", "VAL").await,
        EpicsValue::Double(102.0),
        "and the record computes with the put value"
    );
}

/// A constant input is a SUCCESSFUL read that delivers nothing — it must not
/// gate `fetch_values`. sub aborts on the first FAILED link
/// (`AbortOnFirstFailure`), so if a constant read were classified as a failure
/// the record body would never run. Boundary: constant (success) vs a link to a
/// missing record (failure).
#[epics_macros_rs::epics_test]
async fn a_constant_input_does_not_gate_the_record_body() {
    let db = PvDatabase::new();
    let mut sel = SelRecord::default();
    sel.selm = 0; // Specified
    sel.nvl = "2".to_string(); // constant NVL → SELN=2 at init
    sel.inpa = "11".to_string();
    sel.inpb = "22".to_string();
    sel.inpc = "33".to_string();
    db.add_record("S1", Box::new(sel)).await.unwrap();

    assert_eq!(
        field(&db, "S1", "SELN").await,
        EpicsValue::UShort(2),
        "constant NVL seeds SELN at init (selRecord.c:99)"
    );
    process(&db, "S1").await;
    // C: fetch_values succeeds (constant links deliver nothing, status 0), so
    // do_sel runs and VAL = INP[SELN] = C = 33. softIoc-verified.
    assert_eq!(
        field(&db, "S1", "VAL").await,
        EpicsValue::Double(33.0),
        "a constant NVL/INPn must not fail the fetch gate"
    );
}

/// seq: constant SELL → SELN and constant DOLn → DOn, both init-only
/// (`seqRecord.c:121-126`).
#[epics_macros_rs::epics_test]
async fn seq_seeds_constant_sell_and_doln_at_init() {
    let db = PvDatabase::new();
    let seq = SeqRecord {
        sell: "3".to_string(),
        dol1: "4.5".to_string(),
        ..Default::default()
    };
    db.add_record("Q1", Box::new(seq)).await.unwrap();

    assert_eq!(field(&db, "Q1", "SELN").await, EpicsValue::UShort(3));
    assert_eq!(field(&db, "Q1", "DO1").await, EpicsValue::Double(4.5));

    // A client put to SELN stands — the constant SELL does not re-deliver.
    db.put_pv("Q1.SELN", EpicsValue::UShort(1)).await.unwrap();
    process(&db, "Q1").await;
    assert_eq!(
        field(&db, "Q1", "SELN").await,
        EpicsValue::UShort(1),
        "a constant SELL must not stomp a caput to SELN on process"
    );
}

/// printf is the declared exception: `GET_PRINT` (`printfRecord.c:49-52`)
/// re-runs `recGblInitConstantLink` on every `doPrintf`, so ITS constants keep
/// delivering each cycle and its formatted VAL is stable.
#[epics_macros_rs::epics_test]
async fn printf_constants_still_deliver_every_cycle() {
    let db = PvDatabase::new();
    let mut p = PrintfRecord::default();
    p.fmt = "%g-%g".into();
    p.inp_links[0] = "7".to_string();
    p.inp_links[1] = "8".to_string();
    db.add_record("P1", Box::new(p)).await.unwrap();

    // printf's VAL is a long string (DBF_CHAR array).
    let text = |v: EpicsValue| match v {
        EpicsValue::CharArray(b) => String::from_utf8_lossy(
            &b.iter()
                .copied()
                .take_while(|&c| c != 0)
                .collect::<Vec<_>>(),
        )
        .into_owned(),
        EpicsValue::String(s) => s.as_str_lossy().into_owned(),
        other => panic!("unexpected printf VAL {other:?}"),
    };

    process(&db, "P1").await;
    assert_eq!(
        text(field(&db, "P1", "VAL").await),
        "7-8",
        "printf formats its constant inputs"
    );

    // Even after a put to the fetch sink, printf re-reads the constants.
    db.put_pv("P1.A", EpicsValue::Double(99.0)).await.unwrap();
    process(&db, "P1").await;
    assert_eq!(
        text(field(&db, "P1", "VAL").await),
        "7-8",
        "printf re-loads its constants every cycle (C GET_PRINT), unlike every other record"
    );
}
