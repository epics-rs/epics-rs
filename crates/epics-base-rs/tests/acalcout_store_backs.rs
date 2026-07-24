//! R11-9 — an aCalc store expression (`AA := ...`, `A := ...`) writes the RECORD's
//! field, and the array stores it made are reported in AMASK.
//!
//! C hands `aCalcPerform` pointers to the record's own `a..p` and `aa..ll`
//! (`aCalcoutRecord.c:1283-1285`), so a store opcode writes the record in place
//! (`aCalcPerform.c:456-491`). Array stores additionally set `*amask |= 1<<i`
//! (`:487`, `:524`), and `afterCalc` posts exactly the flagged fields (`:293-297`).
//!
//! The port evaluated over a CLONE of the fields and kept only the stack top, so
//! every store was discarded: `AA := BB*2; SUM(AA)` left AA untouched, AMASK 0 — and
//! `AA := 5` wrote the SCALAR variable A instead of the array.
//!
//! Every expectation below is the output of a driver compiled from
//! `/home/stevek/work/epics-modules/calc/calcApp/src/{aCalcPerform,aCalcPostfix,calcUtil}.c`.

// RTEMS-EXEC-MODEL-ALLOW(6): checked - these run and pass in the feature-ON suite.

use std::collections::HashSet;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::record::Record;
use epics_base_rs::server::records::acalcout::AcalcoutRecord;
use epics_base_rs::types::EpicsValue;

async fn process(db: &PvDatabase, rec: &str) {
    let mut visited = HashSet::new();
    db.process_record_with_links(rec, &mut visited, 0)
        .await
        .unwrap();
}

async fn field(db: &PvDatabase, rec: &str, f: &str) -> EpicsValue {
    let inst = db.get_record(rec).unwrap();
    let g = inst.read();
    g.record.get_field(f).unwrap()
}

/// A 4-element aCalcout with the given CALC and AA/BB seeded.
async fn acalcout(db: &PvDatabase, name: &str, calc: &str) {
    let mut a = AcalcoutRecord::new();
    a.put_field("NELM", EpicsValue::ULong(4)).unwrap();
    a.put_field("CALC", EpicsValue::String(calc.into()))
        .unwrap();
    a.special("CALC", true).unwrap();
    a.put_field("AA", EpicsValue::DoubleArray(vec![9.0, 9.0, 9.0, 9.0]))
        .unwrap();
    a.put_field("BB", EpicsValue::DoubleArray(vec![1.0, 2.0, 3.0, 4.0]))
        .unwrap();
    db.add_record(name, Box::new(a)).await.unwrap();
}

/// Compiled C, arraySize 4, AA=[9,9,9,9], BB=[1,2,3,4], `AA:=BB*2;SUM(AA)`:
///   status=0 amask=0x1 dresult=20 AA=[2,4,6,8]
#[tokio::test]
async fn r11_9_array_store_writes_the_record_field_and_sets_amask() {
    let db = PvDatabase::new();
    acalcout(&db, "S1", "AA:=BB*2;SUM(AA)").await;

    process(&db, "S1").await;

    assert_eq!(
        field(&db, "S1", "AA").await,
        EpicsValue::DoubleArray(vec![2.0, 4.0, 6.0, 8.0]),
        "the store must land in the record's AA"
    );
    assert_eq!(
        field(&db, "S1", "AMASK").await,
        EpicsValue::ULong(0x1),
        "bit 0 = AA was stored into"
    );
    assert_eq!(field(&db, "S1", "VAL").await.to_f64().unwrap(), 20.0);
}

/// A SCALAR stored into an array variable is BROADCAST across the buffer
/// (`aCalcPerform.c:485`, `pd[j] = ps->d`) — it is not `toArray`'d and it does not
/// go to the scalar variable of the same letter. Compiled C, `AA:=5;SUM(AA)`:
/// AA=[5,5,5,5], dresult=20, amask=0x1.
#[tokio::test]
async fn r11_9_a_scalar_stored_into_an_array_variable_is_broadcast() {
    let db = PvDatabase::new();
    acalcout(&db, "S2", "AA:=5;SUM(AA)").await;

    process(&db, "S2").await;

    assert_eq!(
        field(&db, "S2", "AA").await,
        EpicsValue::DoubleArray(vec![5.0, 5.0, 5.0, 5.0])
    );
    assert_eq!(field(&db, "S2", "VAL").await.to_f64().unwrap(), 20.0);
    assert_eq!(
        field(&db, "S2", "A").await.to_f64().unwrap(),
        0.0,
        "`AA := 5` must not write the SCALAR variable A"
    );
    assert_eq!(field(&db, "S2", "AMASK").await, EpicsValue::ULong(0x1));
}

/// Two array stores set two bits. Compiled C, `CC:=AA;DD:=BB;0`: amask=0xc.
#[tokio::test]
async fn r11_9_amask_carries_one_bit_per_stored_array() {
    let db = PvDatabase::new();
    acalcout(&db, "S3", "CC:=AA;DD:=BB;0").await;

    process(&db, "S3").await;

    assert_eq!(
        field(&db, "S3", "CC").await,
        EpicsValue::DoubleArray(vec![9.0, 9.0, 9.0, 9.0])
    );
    assert_eq!(
        field(&db, "S3", "DD").await,
        EpicsValue::DoubleArray(vec![1.0, 2.0, 3.0, 4.0])
    );
    assert_eq!(
        field(&db, "S3", "AMASK").await,
        EpicsValue::ULong(0xc),
        "bits 2 and 3 = CC and DD"
    );
}

/// Negative control, and the reason AMASK must be RESET rather than accumulated:
/// C zeroes `*amask` at the top of every aCalcPerform (`:326`), and the pointer it
/// is handed IS the record's field. So an expression that stores nothing leaves
/// AMASK 0 — even right after a process that set it.
#[tokio::test]
async fn r11_9_amask_is_reset_each_process_not_accumulated() {
    let db = PvDatabase::new();
    acalcout(&db, "S4", "AA:=BB*2;SUM(AA)").await;
    process(&db, "S4").await;
    assert_eq!(field(&db, "S4", "AMASK").await, EpicsValue::ULong(0x1));

    // Same record, an expression with no store at all.
    {
        let inst = db.get_record("S4").unwrap();
        let mut g = inst.write();
        g.record
            .put_field("CALC", EpicsValue::String("SUM(BB)".into()))
            .unwrap();
        g.record.special("CALC", true).unwrap();
    }
    process(&db, "S4").await;

    assert_eq!(
        field(&db, "S4", "AMASK").await,
        EpicsValue::ULong(0),
        "no store this pass: the previous pass's bit must not survive"
    );
    assert_eq!(field(&db, "S4", "VAL").await.to_f64().unwrap(), 10.0);
}

/// A scalar store writes A..L and does NOT touch AMASK — the mask is about ARRAY
/// fields only (C sets it in `STORE_AA..LL` and `A_ASTORE`, never in `STORE_A..P`).
/// Compiled C, `A:=SUM(BB);A`: dresult=10, A=10, amask=0x0.
#[tokio::test]
async fn r11_9_scalar_store_writes_the_field_but_not_amask() {
    let db = PvDatabase::new();
    acalcout(&db, "S5", "A:=SUM(BB);A").await;

    process(&db, "S5").await;

    assert_eq!(field(&db, "S5", "A").await.to_f64().unwrap(), 10.0);
    assert_eq!(field(&db, "S5", "VAL").await.to_f64().unwrap(), 10.0);
    assert_eq!(
        field(&db, "S5", "AMASK").await,
        EpicsValue::ULong(0),
        "AMASK tracks ARRAY stores only"
    );
}

/// The store is sequenced BEFORE the failure, and C's status is deferred to the end
/// of the expression (`aCalcPerform.c:1602-1605`) — so a run that ultimately fails
/// still leaves its stores in the record. Only VAL/AVAL are withheld.
///
/// `AA:=BB*2;SQRT(CC-1)` — CC is all zeros, so every element of `CC-1` is negative
/// and the ARRAY SQRT domain guard sets status -1 (`:775-812`), after AA is written.
#[tokio::test]
async fn r11_9_stores_survive_a_failing_expression() {
    let db = PvDatabase::new();
    acalcout(&db, "S6", "AA:=BB*2;SQRT(CC-1)").await;

    process(&db, "S6").await;

    assert_eq!(
        field(&db, "S6", "AA").await,
        EpicsValue::DoubleArray(vec![2.0, 4.0, 6.0, 8.0]),
        "the store landed before the domain error, and C never rolls it back"
    );
    assert_eq!(field(&db, "S6", "AMASK").await, EpicsValue::ULong(0x1));
    assert_eq!(
        field(&db, "S6", "CSTAT").await.to_f64().unwrap(),
        -1.0,
        "the failing SQRT is still C's -1 (CALC_ALARM)"
    );
}
