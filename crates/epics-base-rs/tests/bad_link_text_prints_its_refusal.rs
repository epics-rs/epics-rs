//! A link put naming no registered JSON link type prints C's `dbJLinkInit`
//! line once, after the put's gate drops, and keeps the old link. softIoc
//! R7.0.10-146 for `dbpf X.SDIS '{"zz":1}'` on a calc whose SDIS is `D`:
//! `dbJLinkInit: Link type 'zz' not found`, SDIS still `D NPP NMS`; the same
//! for `{"zz":}`, `{"zz":1 /*}` and `{/*x*/"zz":1 /*}`. A `.db`
//! carrying the same text prints the line too.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use epics_base_rs::runtime::log::{errlog_add_listener, errlog_flush};
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::{EpicsValue, PvString};

const ZZ: &str = "dbJLinkInit: Link type 'zz' not found";

fn listen() -> impl Fn() -> Vec<String> {
    let heard = Arc::new(Mutex::new(Vec::<String>::new()));
    let sink = Arc::clone(&heard);
    errlog_add_listener(move |m| sink.lock().unwrap().push(m.to_string()));
    move || {
        errlog_flush();
        let mut heard = heard.lock().unwrap();
        let said = heard
            .iter()
            .filter(|m| m.contains("Link type 'zz'"))
            .map(|m| m.trim_end().to_string())
            .collect();
        heard.clear();
        said
    }
}

fn text(s: &str) -> EpicsValue {
    EpicsValue::String(PvString::from(s))
}

fn sdis(db: &PvDatabase) -> String {
    db.get_pv("X.SDIS").unwrap().to_string()
}

#[epics_macros_rs::epics_test]
async fn a_put_of_an_unknown_json_link_type_prints_the_line_after_the_gate() {
    let db = IocBuilder::new()
        .db_string(
            "record(calc, \"X\") { field(SDIS, \"D\") }\nrecord(ai, \"D\") {}\n",
            &HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;
    let heard = listen();
    let kept = sdis(&db);

    db.put_record_field_from_ca("X", "SDIS", text(r#"{"zz":1}"#))
        .await
        .expect_err("C refuses the parse");
    assert_eq!(heard(), [ZZ], "put_record_field_from_ca");
    assert_eq!(sdis(&db), kept, "the old link stays");

    // The key is looked up before the parse fails, so C reports the type for
    // these too; with a type it does serve, it prints yajl's error instead.
    for bad in [r#"{"zz":}"#, r#"{"zz":1 /*}"#, r#"{/*x*/"zz":1 /*}"#] {
        db.put_record_field_from_ca("X", "SDIS", text(bad))
            .await
            .expect_err("C refuses the parse");
        assert_eq!(heard(), [ZZ], "{bad}");
        assert_eq!(sdis(&db), kept, "{bad}: the old link stays");
    }
    for bad in [r#"{"const":1 /*}"#, r#"{/*}"#] {
        db.put_record_field_from_ca("X", "SDIS", text(bad))
            .await
            .expect_err("C refuses the parse");
        assert_eq!(heard(), Vec::<String>::new(), "{bad}");
    }

    db.put_pv_no_process("X.SDIS", text(r#"{"zz":1}"#))
        .await
        .expect_err("C refuses the parse");
    assert_eq!(heard(), [ZZ], "put_pv_no_process");
    assert_eq!(sdis(&db), kept, "the old link stays");

    db.put_many_locked(&["X"], 1, |puts| {
        let x = puts.record(0).unwrap();
        puts.put_record_field_from_ca_no_notify(x, "SDIS", text(r#"{"zz":1}"#))
            .expect_err("C refuses the parse");
        assert_eq!(
            heard(),
            Vec::<String>::new(),
            "nothing is printed under the hold"
        );
    });
    assert_eq!(heard(), [ZZ], "put_many_locked prints it after");
}

#[epics_macros_rs::epics_test]
async fn a_db_load_of_an_unknown_json_link_type_prints_the_line() {
    let heard = listen();
    let builder = IocBuilder::new()
        .db_string(
            "record(ai, \"J1\") { field(INP, \"{\\\"zz\\\":1}\") }\n",
            &HashMap::new(),
        )
        .unwrap();
    let _ = builder.build().await;
    assert_eq!(heard(), [ZZ]);
}
