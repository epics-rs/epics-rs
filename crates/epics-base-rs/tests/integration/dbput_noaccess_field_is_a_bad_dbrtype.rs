//! A put into a `DBF_NOACCESS` field is `dbPut`'s `S_db_badDbrtype` and its
//! print (`field_type > DBF_DEVICE`, `dbAccess.c:1340-1347`), ahead of the
//! `SPC_NOMOD` every such dbCommon field also carries: that refusal is
//! `dbPutSpecial(paddr, 0)` at `:1349-1350`, after this gate. `dbPutField`
//! routes only `DBF_INLINK`..`DBF_FWDLINK` to `dbPutFieldLink`
//! (`:1261-1262`), so a NOACCESS field reaches the same `dbPut` gate from
//! every put entry and from a DB OUT link.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use epics_base_rs::error::CaError;
use epics_base_rs::runtime::log::{errlog_add_listener, errlog_flush};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

fn line(dbr: i16, pv: &str) -> String {
    format!(
        "recGblDbaddrError: dbPut: dbrType = {dbr}, field_type = DBF_NOACCESS (17). \
         Illegal Database Request Type PV: {pv}"
    )
}

#[epics_macros_rs::epics_test]
async fn a_noaccess_field_put_is_a_bad_dbrtype_on_every_entry() {
    let db = IocBuilder::new()
        .db_string(
            r#"
record(ai, "R") {}
record(ao, "W") { field(OUT, "R.BKPT") }
"#,
            &HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;
    db.scan_control().run();
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

    let refused = |r: Result<(), CaError>, what: &str| {
        let err = r.unwrap_err();
        assert!(matches!(err, CaError::BadDbrType(_)), "{what}: {err:?}");
    };
    refused(
        db.put_pv_no_process("R.BKPT", EpicsValue::Double(1.0))
            .await,
        "put_pv_no_process",
    );
    assert_eq!(heard(), [line(10, "R.BKPT")], "put_pv_no_process");
    refused(
        db.put_record_field_from_ca("R", "BKPT", EpicsValue::Double(1.0))
            .await
            .map(|_| ()),
        "put_record_field_from_ca",
    );
    assert_eq!(heard(), [line(10, "R.BKPT")], "put_record_field_from_ca");
    refused(
        db.put_pv_and_post("R.BKPT", EpicsValue::String("1".into()))
            .await,
        "put_pv_and_post",
    );
    assert_eq!(heard(), [line(0, "R.BKPT")], "put_pv_and_post");

    // A DB OUT link: `dbPutLink` -> `dbPut`, the link alarm on W.
    db.put_record_field_from_ca("W", "VAL", EpicsValue::Double(3.0))
        .await
        .unwrap();
    assert_eq!(heard(), [line(10, "R.BKPT")], "W OUT");
    let num = |f: &str| db.get_pv(format!("W.{f}")).unwrap().to_f64();
    assert_eq!([num("STAT"), num("SEVR")], [Some(14.0), Some(3.0)], "W");
}
