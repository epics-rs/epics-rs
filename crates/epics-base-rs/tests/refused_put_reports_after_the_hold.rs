//! A refused put's report is printed by the put's owner once the hold is
//! down, never under it, and names the record as C's `prec->name` does.
//! softIoc R7.0.10 for `record(calc, "C") {field(CALC, "A")}`,
//! `alias("C", "CAL")`, `record(aSub, "S") {}`, `alias("S", "SA")`:
//!
//! ```text
//! dbpf CAL.CALC "A+"
//! recGblRecordError: calc: Illegal CALC field Illegal field value PV: C
//! C.CALC: Incomplete expression, operand missing in expression "A+"
//! dbpf SA.INAM x
//! recGblDbaddrError: dbPut Attempt to modify noMod field PV: S.INAM
//! ```
//!
//! and, on oracleBusyIoc for `record(bo, "B")`, a `DBR_STRING` put of `Bogus`
//! to `B` answers `ECA_PUTFAIL` and prints
//! `recGblRecordError: Bogus Illegal choice PV: B`.
//!
//! The `C.INPA` line is `dbPut`'s link-field refusal, pinned against softIoc
//! in `dbput_link_field_refusal_precedes_nomod.rs`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use epics_base_rs::runtime::log::{errlog_add_listener, errlog_flush};
use epics_base_rs::server::database::PutSplit;
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

const CALC: [&str; 2] = [
    "recGblRecordError: calc: Illegal CALC field Illegal field value PV: C",
    "C.CALC: Incomplete expression, operand missing in expression \"A+\"",
];
const INAM: &str = "recGblDbaddrError: dbPut Attempt to modify noMod field PV: S.INAM";
const CHOICE: &str = "recGblRecordError: Bogus Illegal choice PV: B";
const INPA: &str = "recGblDbaddrError: dbPut: dbrType = 10, field_type = DBF_INLINK (14). \
                    Illegal Database Request Type PV: C.INPA";

fn listen() -> impl Fn() -> Vec<String> {
    let heard = Arc::new(Mutex::new(Vec::<String>::new()));
    let sink = Arc::clone(&heard);
    errlog_add_listener(move |m| sink.lock().unwrap().push(m.trim_end().to_string()));
    move || {
        errlog_flush();
        std::mem::take(&mut *heard.lock().unwrap())
    }
}

async fn ioc() -> Arc<PvDatabase> {
    IocBuilder::new()
        .db_string(
            r#"
record(calc, "C") { field(CALC, "A") }
alias("C", "CAL")
record(aSub, "S") {}
alias("S", "SA")
record(bo, "B") { field(ZNAM, "Off") field(ONAM, "On") }
alias("B", "BA")
"#,
            &HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap()
        .0
}

fn text(s: &str) -> EpicsValue {
    EpicsValue::String(s.into())
}

#[epics_macros_rs::epics_test]
async fn a_refused_put_reports_after_the_hold_by_its_canonical_name() {
    let db = ioc().await;
    let heard = listen();

    db.put_record_field_from_ca_no_notify("CAL", "CALC", text("A+"))
        .await
        .expect_err("calc refuses the CALC");
    assert_eq!(heard(), CALC);
    db.put_record_field_from_ca_no_notify("SA", "INAM", text("x"))
        .await
        .expect_err("INAM is SPC_NOMOD");
    assert_eq!(heard(), [INAM]);
    db.put_record_field_from_ca_no_notify("BA", "VAL", text("Bogus"))
        .await
        .expect_err("Bogus is no state of B");
    assert_eq!(heard(), [CHOICE]);
}

#[epics_macros_rs::epics_test]
async fn nothing_is_printed_under_the_hold() {
    let db = ioc().await;
    let heard = listen();

    db.put_many_locked(&["C", "S", "B"], 3, |puts| {
        let c = puts.record(0).unwrap();
        let s = puts.record(1).unwrap();
        let b = puts.record(2).unwrap();
        puts.put_record_field_from_ca_no_notify(c, "CALC", text("A+"), PutSplit::DbPutField)
            .expect_err("special() refuses");
        puts.put_pv(s, "INAM", text("x"))
            .expect_err("the SPC_NOMOD gate refuses");
        puts.put_pv(c, "INPA", EpicsValue::Double(1.0))
            .expect_err("the link-field gate refuses");
        puts.put_record_field_from_ca_no_notify(b, "VAL", text("Bogus"), PutSplit::DbPutField)
            .expect_err("putStringEnum refuses");
        assert_eq!(heard(), Vec::<String>::new());
    });
    assert_eq!(heard(), [CALC[0], CALC[1], INAM, INPA, CHOICE]);
}
