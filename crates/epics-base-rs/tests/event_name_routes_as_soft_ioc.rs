//! `post_event_named` resolves an event name the way C `eventNameToHandle`
//! does (`dbScan.c:469-533`), on the poster's name and on each record's EVNT.
//!
//! Oracle — softIoc 7.0.10.1-DEV, with `SRC` a calc answering 5 and `SRCS` a
//! stringin holding `" myEv "`, each feeding an event record's INP:
//!
//! ```text
//! record(calc, "T5") { field(SCAN, "Event") field(EVNT, "5.0")  field(CALC, "VAL+1") }
//! record(calc, "TS") { field(SCAN, "Event") field(EVNT, " 5 ")  field(CALC, "VAL+1") }
//! record(calc, "T6") { field(SCAN, "Event") field(EVNT, "6")    field(CALC, "VAL+1") }
//! record(calc, "TZ") { field(SCAN, "Event") field(EVNT, "0.5")  field(CALC, "VAL+1") }
//! record(calc, "TN") { field(SCAN, "Event") field(EVNT, "myEv") field(CALC, "VAL+1") }
//!
//! dbpf EV.PROC 1; dbpf EV.PROC 1; dbpf EVS.PROC 1
//! dbgf EV  -> "5"        dbgf EVS -> " myEv "
//! dbgf T5  -> 2          dbgf TS  -> 2
//! dbgf T6  -> 0          dbgf TZ  -> 0
//! dbgf TN  -> 1
//! ```

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::record::{Record, ScanType};
use epics_base_rs::server::records::calc::CalcRecord;
use epics_base_rs::types::EpicsValue;

#[epics_macros_rs::epics_test]
async fn event_names_fold_as_soft_ioc_folds_them() {
    let db = PvDatabase::new();
    let targets = [
        ("T5", "5.0"),
        ("TS", " 5 "),
        ("T6", "6"),
        ("TZ", "0.5"),
        ("TN", "myEv"),
    ];
    for (name, evnt) in targets {
        let mut calc = CalcRecord::default();
        calc.put_field("CALC", EpicsValue::String("VAL+1".into()))
            .unwrap();
        db.add_record(name, Box::new(calc)).await.unwrap();
        {
            let rec = db.get_record(name).unwrap();
            let mut w = rec.write();
            w.common.scan = ScanType::Event;
            w.common.evnt = evnt.to_string();
        }
        db.update_scan_index(name, ScanType::Passive, ScanType::Event, 0, 0);
    }

    // What EV and EVS post: their VAL, as C reads it from INP.
    for posted in ["5", "5", " myEv "] {
        db.post_event_named(posted).await;
    }

    for (name, want) in [
        ("T5", 2.0),
        ("TS", 2.0),
        ("T6", 0.0),
        ("TZ", 0.0),
        ("TN", 1.0),
    ] {
        let val = db.get_record(name).unwrap().read().record.get_field("VAL");
        assert_eq!(val, Some(EpicsValue::Double(want)), "{name}.VAL");
    }
}

/// `eventNameToHandle` parses with `epicsParseDouble`, i.e. `strtod`, so a
/// hexadecimal name is a numeric event. Oracle — softIoc 7.0.10.1-DEV, event
/// records posting a stringin's `"0x10"` and then `"16"`:
///
/// ```text
/// record(calc, "TH") { field(SCAN, "Event") field(EVNT, "0x10")  field(CALC, "VAL+1") }
/// record(calc, "TD") { field(SCAN, "Event") field(EVNT, "16")    field(CALC, "VAL+1") }
/// record(calc, "TF") { field(SCAN, "Event") field(EVNT, "0x1p4") field(CALC, "VAL+1") }
/// record(calc, "TA") { field(SCAN, "Event") field(EVNT, "alpha") field(CALC, "VAL+1") }
///
/// post "0x10": TH 1, TD 1, TF 1;  then post "16": TH 2, TD 2, TF 2, TA 0
/// ```
#[epics_macros_rs::epics_test]
async fn a_hexadecimal_event_name_is_its_event_number() {
    let db = PvDatabase::new();
    for (name, evnt) in [
        ("TH", "0x10"),
        ("TD", "16"),
        ("TF", "0x1p4"),
        ("TA", "alpha"),
    ] {
        let mut calc = CalcRecord::default();
        calc.put_field("CALC", EpicsValue::String("VAL+1".into()))
            .unwrap();
        db.add_record(name, Box::new(calc)).await.unwrap();
        {
            let rec = db.get_record(name).unwrap();
            let mut w = rec.write();
            w.common.scan = ScanType::Event;
            w.common.evnt = evnt.to_string();
        }
        db.update_scan_index(name, ScanType::Passive, ScanType::Event, 0, 0);
    }

    let val = |name: &str| db.get_record(name).unwrap().read().record.get_field("VAL");
    db.post_event_named("0x10").await;
    for name in ["TH", "TD", "TF"] {
        assert_eq!(
            val(name),
            Some(EpicsValue::Double(1.0)),
            "{name}.VAL after 0x10"
        );
    }
    db.post_event_named("16").await;
    for (name, want) in [("TH", 2.0), ("TD", 2.0), ("TF", 2.0), ("TA", 0.0)] {
        assert_eq!(
            val(name),
            Some(EpicsValue::Double(want)),
            "{name}.VAL after 16"
        );
    }
}
