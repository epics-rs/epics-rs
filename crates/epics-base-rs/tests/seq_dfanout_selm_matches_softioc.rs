//! seq and dfanout SELM/SELN selection and seq's DLY ordering, against
//! softIoc R7.0.10: the values each case writes into a sink, in the order its
//! camonitor shows them, and the record's SEVR after the case. softIoc and
//! softioc-rs print the same camonitor and `dbgf` lines for this database;
//! K:DOLS's sink reads 300..315 from both.
//! seq's SHFT is -1 by default, so a Mask SELN of 1 selects group 1.

use std::collections::HashMap;
use std::time::Duration;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::event_queue::EventReader;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::server::record::AlarmSeverity;
use epics_base_rs::types::{DbFieldType, EpicsValue, PvString};

const DB: &str = r#"
record(calc, "K:SINK") { field(CALC, "A") field(MDEL, "-1") }
record(seq, "K:SEQ") { field(DO0, "100") field(LNK0, "K:SINK.A PP") field(DO1, "101") field(LNK1, "K:SINK.A PP") field(DO2, "102") field(LNK2, "K:SINK.A PP") field(DO3, "103") field(LNK3, "K:SINK.A PP") field(DO4, "104") field(LNK4, "K:SINK.A PP") field(DO5, "105") field(LNK5, "K:SINK.A PP") field(DO6, "106") field(LNK6, "K:SINK.A PP") field(DO7, "107") field(LNK7, "K:SINK.A PP") field(DO8, "108") field(LNK8, "K:SINK.A PP") field(DO9, "109") field(LNK9, "K:SINK.A PP") field(DOA, "110") field(LNKA, "K:SINK.A PP") field(DOB, "111") field(LNKB, "K:SINK.A PP") field(DOC, "112") field(LNKC, "K:SINK.A PP") field(DOD, "113") field(LNKD, "K:SINK.A PP") field(DOE, "114") field(LNKE, "K:SINK.A PP") field(DOF, "115") field(LNKF, "K:SINK.A PP") }
record(seq, "K:ORD") { field(SELM, "All") field(DO0,"200") field(DLY0,"0.3") field(LNK0,"K:SINK.A PP") field(DO1,"201") field(DLY1,"0") field(LNK1,"K:SINK.A PP") field(DO2,"202") field(DLY2,"0.1") field(LNK2,"K:SINK.A PP") }
record(calc, "K:DSINK") { field(CALC, "A") field(MDEL, "-1") }
record(calcout, "K:OA") { field(CALC, "0") field(OUT, "K:DSINK.A PP") field(OOPT, "Every Time") }
record(calcout, "K:OB") { field(CALC, "1") field(OUT, "K:DSINK.A PP") field(OOPT, "Every Time") }
record(calcout, "K:OC") { field(CALC, "2") field(OUT, "K:DSINK.A PP") field(OOPT, "Every Time") }
record(calcout, "K:OD") { field(CALC, "3") field(OUT, "K:DSINK.A PP") field(OOPT, "Every Time") }
record(calcout, "K:OE") { field(CALC, "4") field(OUT, "K:DSINK.A PP") field(OOPT, "Every Time") }
record(calcout, "K:OF") { field(CALC, "5") field(OUT, "K:DSINK.A PP") field(OOPT, "Every Time") }
record(calcout, "K:OG") { field(CALC, "6") field(OUT, "K:DSINK.A PP") field(OOPT, "Every Time") }
record(calcout, "K:OH") { field(CALC, "7") field(OUT, "K:DSINK.A PP") field(OOPT, "Every Time") }
record(calcout, "K:OI") { field(CALC, "8") field(OUT, "K:DSINK.A PP") field(OOPT, "Every Time") }
record(calcout, "K:OJ") { field(CALC, "9") field(OUT, "K:DSINK.A PP") field(OOPT, "Every Time") }
record(calcout, "K:OK") { field(CALC, "10") field(OUT, "K:DSINK.A PP") field(OOPT, "Every Time") }
record(calcout, "K:OL") { field(CALC, "11") field(OUT, "K:DSINK.A PP") field(OOPT, "Every Time") }
record(calcout, "K:OM") { field(CALC, "12") field(OUT, "K:DSINK.A PP") field(OOPT, "Every Time") }
record(calcout, "K:ON") { field(CALC, "13") field(OUT, "K:DSINK.A PP") field(OOPT, "Every Time") }
record(calcout, "K:OO") { field(CALC, "14") field(OUT, "K:DSINK.A PP") field(OOPT, "Every Time") }
record(calcout, "K:OP") { field(CALC, "15") field(OUT, "K:DSINK.A PP") field(OOPT, "Every Time") }
record(dfanout, "K:FAN") { field(OUTA, "K:OA.PROC") field(OUTB, "K:OB.PROC") field(OUTC, "K:OC.PROC") field(OUTD, "K:OD.PROC") field(OUTE, "K:OE.PROC") field(OUTF, "K:OF.PROC") field(OUTG, "K:OG.PROC") field(OUTH, "K:OH.PROC") field(OUTI, "K:OI.PROC") field(OUTJ, "K:OJ.PROC") field(OUTK, "K:OK.PROC") field(OUTL, "K:OL.PROC") field(OUTM, "K:OM.PROC") field(OUTN, "K:ON.PROC") field(OUTO, "K:OO.PROC") field(OUTP, "K:OP.PROC") }
record(ai, "K:V0") { field(VAL, "300") }
record(ai, "K:V1") { field(VAL, "301") }
record(ai, "K:V2") { field(VAL, "302") }
record(ai, "K:V3") { field(VAL, "303") }
record(ai, "K:V4") { field(VAL, "304") }
record(ai, "K:V5") { field(VAL, "305") }
record(ai, "K:V6") { field(VAL, "306") }
record(ai, "K:V7") { field(VAL, "307") }
record(ai, "K:V8") { field(VAL, "308") }
record(ai, "K:V9") { field(VAL, "309") }
record(ai, "K:VA") { field(VAL, "310") }
record(ai, "K:VB") { field(VAL, "311") }
record(ai, "K:VC") { field(VAL, "312") }
record(ai, "K:VD") { field(VAL, "313") }
record(ai, "K:VE") { field(VAL, "314") }
record(ai, "K:VF") { field(VAL, "315") }
record(seq, "K:DOLS") { field(SELM, "All") field(DOL0, "K:V0") field(LNK0, "K:SINK.A PP") field(DOL1, "K:V1") field(LNK1, "K:SINK.A PP") field(DOL2, "K:V2") field(LNK2, "K:SINK.A PP") field(DOL3, "K:V3") field(LNK3, "K:SINK.A PP") field(DOL4, "K:V4") field(LNK4, "K:SINK.A PP") field(DOL5, "K:V5") field(LNK5, "K:SINK.A PP") field(DOL6, "K:V6") field(LNK6, "K:SINK.A PP") field(DOL7, "K:V7") field(LNK7, "K:SINK.A PP") field(DOL8, "K:V8") field(LNK8, "K:SINK.A PP") field(DOL9, "K:V9") field(LNK9, "K:SINK.A PP") field(DOLA, "K:VA") field(LNKA, "K:SINK.A PP") field(DOLB, "K:VB") field(LNKB, "K:SINK.A PP") field(DOLC, "K:VC") field(LNKC, "K:SINK.A PP") field(DOLD, "K:VD") field(LNKD, "K:SINK.A PP") field(DOLE, "K:VE") field(LNKE, "K:SINK.A PP") field(DOLF, "K:VF") field(LNKF, "K:SINK.A PP") }
"#;

/// `(SELM, SELN, sink values in order, SEVR)` per seq case.
const SEQ: &[(&str, u16, &[i32], AlarmSeverity)] = &[
    (
        "All",
        0,
        &[
            100, 101, 102, 103, 104, 105, 106, 107, 108, 109, 110, 111, 112, 113, 114, 115,
        ],
        AlarmSeverity::NoAlarm,
    ),
    ("Specified", 0, &[100], AlarmSeverity::NoAlarm),
    ("Specified", 1, &[101], AlarmSeverity::NoAlarm),
    ("Specified", 15, &[115], AlarmSeverity::NoAlarm),
    ("Specified", 16, &[], AlarmSeverity::Invalid),
    ("Specified", 17, &[], AlarmSeverity::Invalid),
    ("Specified", 65535, &[], AlarmSeverity::Invalid),
    ("Mask", 0, &[], AlarmSeverity::NoAlarm),
    ("Mask", 1, &[101], AlarmSeverity::NoAlarm),
    ("Mask", 32769, &[101], AlarmSeverity::NoAlarm),
    (
        "Mask",
        65535,
        &[
            101, 102, 103, 104, 105, 106, 107, 108, 109, 110, 111, 112, 113, 114, 115,
        ],
        AlarmSeverity::NoAlarm,
    ),
];

/// The same for dfanout; the sink sees the index of each OUTn driven.
const DFANOUT: &[(&str, u16, &[i32], AlarmSeverity)] = &[
    (
        "All",
        0,
        &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15],
        AlarmSeverity::NoAlarm,
    ),
    ("Specified", 0, &[], AlarmSeverity::NoAlarm),
    ("Specified", 1, &[0], AlarmSeverity::NoAlarm),
    ("Specified", 16, &[15], AlarmSeverity::NoAlarm),
    ("Specified", 17, &[], AlarmSeverity::Invalid),
    ("Specified", 65535, &[], AlarmSeverity::Invalid),
    ("Mask", 0, &[], AlarmSeverity::NoAlarm),
    ("Mask", 1, &[0], AlarmSeverity::NoAlarm),
    ("Mask", 32769, &[0, 15], AlarmSeverity::NoAlarm),
    (
        "Mask",
        65535,
        &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15],
        AlarmSeverity::NoAlarm,
    ),
];

/// K:DOLS reads each DOLn from its own source into DOn and writes it.
const READ: &[i32] = &[
    300, 301, 302, 303, 304, 305, 306, 307, 308, 309, 310, 311, 312, 313, 314, 315,
];

/// K:ORD's groups in index order whatever their DLYs (0.3, 0, 0.1).
const ORDERED: &[i32] = &[200, 201, 202];

fn text(s: &str) -> EpicsValue {
    EpicsValue::String(PvString::from(s))
}

async fn put(db: &PvDatabase, rec: &str, field: &str, value: EpicsValue) {
    db.put_record_field_from_ca(rec, field, value)
        .await
        .unwrap_or_else(|e| panic!("{rec}.{field}: {e}"));
}

fn watch(db: &PvDatabase, rec: &str) -> EventReader {
    db.get_record(rec)
        .unwrap()
        .write()
        .add_subscriber("VAL", 1, DbFieldType::Double, EventMask::VALUE.bits())
        .unwrap()
}

fn drain(rx: &mut EventReader) -> Vec<i32> {
    std::iter::from_fn(|| rx.try_recv().ok())
        .map(|e| e.snapshot.value.to_f64().unwrap() as i32)
        .collect()
}

/// Process `rec` and wait out its PACT (seq is asynchronous).
async fn settle(db: &PvDatabase, rec: &str) {
    for _ in 0..400 {
        if !db.get_record(rec).unwrap().read().is_processing() {
            return;
        }
        epics_base_rs::runtime::task::sleep(Duration::from_millis(5)).await;
    }
    panic!("{rec} never finished");
}

fn sevr(db: &PvDatabase, rec: &str) -> AlarmSeverity {
    db.get_record(rec).unwrap().read().common.sevr
}

async fn build() -> std::sync::Arc<PvDatabase> {
    IocBuilder::new()
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0
}

#[epics_macros_rs::epics_test]
async fn seq_selects_and_orders_its_groups_as_softioc() {
    let db = build().await;
    let mut sink = watch(&db, "K:SINK");
    for &(selm, seln, want, sev) in SEQ {
        put(&db, "K:SEQ", "SELM", text(selm)).await;
        put(&db, "K:SEQ", "SELN", EpicsValue::UShort(seln)).await;
        put(&db, "K:SEQ", "PROC", EpicsValue::Char(1)).await;
        settle(&db, "K:SEQ").await;
        assert_eq!(drain(&mut sink), want, "{selm} {seln}");
        assert_eq!(sevr(&db, "K:SEQ"), sev, "{selm} {seln} SEVR");
    }
    put(&db, "K:ORD", "PROC", EpicsValue::Char(1)).await;
    settle(&db, "K:ORD").await;
    assert_eq!(drain(&mut sink), ORDERED);
}

#[epics_macros_rs::epics_test]
async fn dfanout_selects_its_outputs_as_softioc() {
    let db = build().await;
    let mut sink = watch(&db, "K:DSINK");
    for &(selm, seln, want, sev) in DFANOUT {
        put(&db, "K:FAN", "SELM", text(selm)).await;
        put(&db, "K:FAN", "SELN", EpicsValue::UShort(seln)).await;
        put(&db, "K:FAN", "VAL", EpicsValue::Double(5.0)).await;
        assert_eq!(drain(&mut sink), want, "{selm} {seln}");
        assert_eq!(sevr(&db, "K:FAN"), sev, "{selm} {seln} SEVR");
    }
}

#[epics_macros_rs::epics_test]
async fn seq_reads_each_dol_into_its_group_as_softioc() {
    let db = build().await;
    let mut sink = watch(&db, "K:SINK");
    put(&db, "K:DOLS", "PROC", EpicsValue::Char(1)).await;
    settle(&db, "K:DOLS").await;
    assert_eq!(drain(&mut sink), READ);
}
