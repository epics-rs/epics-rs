//! aSub's OUT push reaches exactly the wired OUTx, pinned to softIoc
//! R7.0.10.1: an empty-SNAM aSub (status 0, VALx 0) over ao targets at 9.

use epics_base_rs::server::database::ProcStack;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

const DB: &str = r#"
record(aSub, "AS") { field(OUTA, "TA") field(OUTB, "TB") field(OUTD, "TD") }
record(ao, "TA") { field(VAL, "9") }
record(ao, "TB") { field(VAL, "9") }
record(ao, "TC") { field(VAL, "9") }
record(ao, "TD") { field(VAL, "9") }
record(ao, "TE") { field(VAL, "9") }
"#;

#[epics_macros_rs::epics_test]
async fn asub_drives_only_its_wired_outs() {
    let db = IocBuilder::new()
        .db_string(DB, &std::collections::HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;
    let vals = async |names: &[&str]| {
        let mut out = Vec::new();
        for n in names {
            out.push(db.get_pv(format!("{n}.VAL")).unwrap());
        }
        out
    };
    let d = EpicsValue::Double;

    db.process_record_with_links("AS", &mut ProcStack::new())
        .await
        .unwrap();
    assert_eq!(
        vals(&["TA", "TB", "TC", "TD", "TE"]).await,
        [d(0.0), d(0.0), d(9.0), d(0.0), d(9.0)]
    );

    db.put_pv_no_process("AS.OUTC", EpicsValue::String("TE".into()))
        .await
        .unwrap();
    db.process_record_with_links("AS", &mut ProcStack::new())
        .await
        .unwrap();
    assert_eq!(vals(&["TC", "TE"]).await, [d(9.0), d(0.0)]);
}
