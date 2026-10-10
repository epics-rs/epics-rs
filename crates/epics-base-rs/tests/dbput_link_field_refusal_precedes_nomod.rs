//! `dbPut` into a link field refuses with `S_db_badDbrtype` and its print
//! before it reaches `SPC_NOMOD` (`dbAccess.c:1340-1347`, then
//! `dbPutSpecial(paddr, 0)` at `:1350`). softIoc R7.0.10 for `ao W1 OUT
//! "A.SUBL"` (aSub SUBL is DBF_INLINK and SPC_NOMOD), `ao W2 OUT "C.INPA"`,
//! `ao W3 OUT "WF.NELM"` and `stringout W4 OUT "B.VAL"`, `dbpf W1 3`,
//! `dbpf W2 4`, `dbpf W3 5`, `dbpf W4 Bogus`:
//!
//! ```text
//! recGblDbaddrError: dbPut: dbrType = 10, field_type = DBF_INLINK (14). Illegal Database Request Type PV: A.SUBL
//! recGblDbaddrError: dbPut: dbrType = 10, field_type = DBF_INLINK (14). Illegal Database Request Type PV: C.INPA
//! recGblDbaddrError: dbPut Attempt to modify noMod field PV: WF.NELM
//! recGblRecordError: Bogus Illegal choice PV: B
//! ```
//!
//! then W1-W3 LINK/INVALID, A.SUBL and C.INPA still "", WF.NELM 4. Each line
//! is the OUT link's, made inside W's cycle; the port prints it once the
//! put's hold is down.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use epics_base_rs::error::CaError;
use epics_base_rs::runtime::log::{errlog_add_listener, errlog_flush};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

const NOMOD: &str = "recGblDbaddrError: dbPut Attempt to modify noMod field PV: WF.NELM";
const CHOICE: &str = "recGblRecordError: Bogus Illegal choice PV: B";

fn line(pv: &str) -> String {
    format!(
        "recGblDbaddrError: dbPut: dbrType = 10, field_type = DBF_INLINK (14). \
         Illegal Database Request Type PV: {pv}"
    )
}

#[epics_macros_rs::epics_test]
async fn a_link_field_is_refused_as_a_bad_dbrtype_before_nomod() {
    let db = IocBuilder::new()
        .db_string(
            r#"
record(aSub, "A") {}
record(calc, "C") {}
record(ao, "W1") { field(OUT, "A.SUBL") }
record(ao, "W2") { field(OUT, "C.INPA") }
record(waveform, "WF") { field(NELM, "4") }
record(ao, "W3") { field(OUT, "WF.NELM") }
record(bo, "B") { field(ZNAM, "Off") field(ONAM, "On") }
record(stringout, "W4") { field(OUT, "B.VAL") }
"#,
            &HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;
    let heard = Arc::new(Mutex::new(Vec::<String>::new()));
    let sink = Arc::clone(&heard);
    errlog_add_listener(move |m| sink.lock().unwrap().push(m.trim_end().to_string()));
    let heard = move || {
        errlog_flush();
        let mut h = heard.lock().unwrap();
        let said: Vec<String> = h
            .iter()
            .filter(|m| m.starts_with("recGbl"))
            .cloned()
            .collect();
        h.clear();
        said
    };

    // A DB OUT link: `dbPutLink` -> `dbPut`.
    for (w, v, pv, said) in [
        ("W1", 3.0, "A.SUBL", line("A.SUBL")),
        ("W2", 4.0, "C.INPA", line("C.INPA")),
        ("W3", 5.0, "WF.NELM", NOMOD.to_string()),
    ] {
        db.put_record_field_from_ca(w, "VAL", EpicsValue::Double(v))
            .await
            .unwrap();
        assert_eq!(heard(), [said], "{w}");
        assert_eq!(
            db.get_pv(&format!("{w}.STAT")).unwrap().to_f64(),
            Some(14.0),
            "{w} LINK"
        );
        assert_eq!(
            db.get_pv(&format!("{w}.SEVR")).unwrap().to_f64(),
            Some(3.0),
            "{w} INVALID"
        );
        let kept = if pv == "WF.NELM" { "4" } else { "" };
        assert_eq!(db.get_pv(pv).unwrap().to_string(), kept, "{pv} kept");
    }
    db.put_record_field_from_ca("W4", "VAL", EpicsValue::String("Bogus".into()))
        .await
        .unwrap();
    assert_eq!(heard(), [CHOICE]);
    assert_eq!(db.get_pv("B.VAL").unwrap().to_f64(), Some(0.0), "B kept");

    // A scan's cycle: the frame that took the gate prints once it is down.
    let mut visited = epics_base_rs::server::database::ProcStack::new();
    db.process_record_with_links("W3", &mut visited)
        .await
        .unwrap();
    assert_eq!(heard(), [NOMOD]);

    // The other `dbPut` body answers the same status and print.
    for pv in ["A.SUBL", "C.INPA"] {
        let err = db
            .put_pv_and_post(pv, EpicsValue::Double(5.0))
            .await
            .unwrap_err();
        assert!(matches!(err, CaError::BadDbrType(_)), "{pv}: {err:?}");
        assert_eq!(heard(), [line(pv)], "{pv}");
    }
}
