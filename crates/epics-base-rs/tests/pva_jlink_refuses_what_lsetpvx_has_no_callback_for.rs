//! pvxs `lsetPVX` leaves `parse_double` and `parse_start_array` NULL
//! (`pvxs/ioc/pvalink_jlif.cpp:286-304`), so `dbJLinkParse` stops on either
//! with `S_db_badField` and prints nothing of its own (`dbJLink.c:55,
//! 425-437`); every other value it logs and takes. softIocPVX (pvxs 1.5.3)
//! loading `field(INP, {pva:{pv:"T:X",Q:7.5}})` prints
//! `Can't set 'R:DBL.INP' to '{pva:{pv:"T:X",Q:7.5}}'  : Bad Field value`
//! and refuses the file; a caput of `{pva:{pv:"T:X",Q:[1]}}` fails and the
//! field keeps its link.

use std::collections::HashMap;

use epics_base_rs::error::CaError;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::record::{LinkFieldType, check_json_link_text};
use epics_base_rs::types::EpicsValue;

fn refused(text: &str) -> Option<String> {
    match check_json_link_text(text, LinkFieldType::In) {
        Err(CaError::BadLink(refusal)) => refusal.report(),
        other => panic!("{text} was not refused as a link: {other:?}"),
    }
}

#[test]
fn a_double_or_an_array_anywhere_in_a_pva_link_is_refused_silently() {
    for text in [
        r#"{pva:{pv:"T:X",Q:7.5}}"#,
        r#"{pva:{pv:"T:X",Q:[1]}}"#,
        r#"{pva:{pv:"T:X",field:{a:1.5}}}"#,
        r#"{pva:{pv:"T:X",field:{a:[]}}}"#,
        r#"{pva:7.5}"#,
    ] {
        assert_eq!(refused(text), None, "{text}");
    }
}

#[test]
fn every_other_pva_value_is_taken() {
    for text in [
        r#"{pva:{pv:"T:X",proc:"BOGUS",zzz:1,sevr:null,time:true}}"#,
        r#"{pva:{pv:"T:X",field:{a:1,b:"s",c:{d:null}}}}"#,
        r#"{pva:"T:X"}"#,
    ] {
        assert!(
            check_json_link_text(text, LinkFieldType::In).is_ok(),
            "{text}"
        );
    }
}

/// Build an IOC serving `pva` from `db` and give back whether it built and
/// what it printed to stderr.
#[cfg(unix)]
async fn build_capturing_stderr(db: &str) -> (bool, String) {
    use std::os::fd::AsRawFd;

    let err_sink = tempfile::NamedTempFile::new().expect("stderr capture");
    // SAFETY: fd 2 is process-global, which is why this test is `serial`.
    let saved_err = unsafe {
        let saved = libc::dup(2);
        libc::dup2(err_sink.as_file().as_raw_fd(), 2);
        saved
    };
    let built = IocBuilder::new()
        .register_link_type("pva", |_db| async { Vec::new() })
        .db_string(db, &HashMap::new())
        .unwrap()
        .build()
        .await
        .is_ok();
    // SAFETY: restoring what this call replaced.
    unsafe {
        libc::dup2(saved_err, 2);
        libc::close(saved_err);
    }
    let err = std::fs::read_to_string(err_sink.path()).expect("stderr capture");
    let mut plain = String::with_capacity(err.len());
    let mut rest = err.as_str();
    while let Some(at) = rest.find('\x1b') {
        plain.push_str(&rest[..at]);
        rest = rest[at..].find('m').map_or("", |end| &rest[at + end + 1..]);
    }
    plain.push_str(rest);
    (built, plain)
}

#[cfg(unix)]
#[epics_base_rs::epics_test]
#[serial_test::serial(db_load_stderr)]
async fn a_pva_link_with_a_double_does_not_load() {
    let (built, err) = build_capturing_stderr(
        r#"record(ai, "R:DBL") { field(INP, {pva:{pv:"T:X",Q:7.5}}) }
record(ai, "R:AFTER") {}"#,
    )
    .await;
    assert!(!built);
    assert_eq!(
        err.lines().next(),
        Some(r#"ERROR: Can't set 'R:DBL.INP' to '{pva:{pv:"T:X",Q:7.5}}'  : Bad Field value"#),
        "{err}"
    );
}

#[epics_base_rs::epics_test]
async fn a_runtime_put_of_a_pva_link_with_an_array_keeps_the_old_link() {
    let (db, _) = IocBuilder::new()
        .register_link_type("pva", |_db| async { Vec::new() })
        .db_string(
            r#"record(ai, "R:OK") { field(INP, "R:OK2") }
record(ai, "R:OK2") {}"#,
            &HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap();
    db.ioc_init().await;
    let put = db
        .put_record_field_from_ca(
            "R:OK",
            "INP",
            EpicsValue::String(r#"{pva:{pv:"T:X",Q:[1]}}"#.into()),
        )
        .await;
    assert!(put.is_err(), "{put:?}");
    let inp = db.get_pv("R:OK.INP").unwrap();
    assert_eq!(inp, EpicsValue::String("R:OK2 NPP NMS".into()));
}

/// An empty pva name is taken at load and at a put, as softIocPVX takes it
/// (`pva_parse_string` stores it, `pvxs/ioc/pvalink_jlif.cpp:147-148`).
#[epics_base_rs::epics_test]
async fn an_empty_pva_name_loads_and_puts() {
    for text in [r#"{pva:{pv:""}}"#, r#"{pva:""}"#] {
        let db = format!(r#"record(ai, "R:E") {{ field(INP, {text}) }}"#);
        let (db, _) = IocBuilder::new()
            .register_link_type("pva", |_db| async { Vec::new() })
            .db_string(&db, &HashMap::new())
            .unwrap_or_else(|e| panic!("{text}: {e:?}"))
            .build()
            .await
            .unwrap_or_else(|e| panic!("{text}: {e:?}"));
        db.ioc_init().await;
        assert!(db.get_record("R:E").is_some(), "{text}");
    }
    let (db, _) = IocBuilder::new()
        .register_link_type("pva", |_db| async { Vec::new() })
        .db_string(
            r#"record(ai, "R:OK") { field(INP, "R:OK2") }
record(ai, "R:OK2") {}"#,
            &HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap();
    db.ioc_init().await;
    for text in [r#"{pva:{pv:""}}"#, r#"{pva:""}"#, r#"{pva:{pv:" ",Q:7}}"#] {
        db.put_record_field_from_ca("R:OK", "INP", EpicsValue::String(text.into()))
            .await
            .unwrap_or_else(|e| panic!("{text}: {e:?}"));
    }
}

/// A null, bool or integer pva root is ignored by `lsetPVX`
/// (`pvxs/ioc/pvalink_jlif.cpp:69-139`): softIocPVX takes `{pva:5}`,
/// `{pva:true}` and `{pva:null}` at load and at a put, as an empty name.
#[epics_base_rs::epics_test]
async fn a_non_string_pva_root_loads_and_puts() {
    for text in ["{pva:5}", "{pva:true}", "{pva:null}"] {
        let db = format!(r#"record(ai, "R:E") {{ field(INP, {text}) }}"#);
        let (db, _) = IocBuilder::new()
            .register_link_type("pva", |_db| async { Vec::new() })
            .db_string(&db, &HashMap::new())
            .unwrap_or_else(|e| panic!("{text}: {e:?}"))
            .build()
            .await
            .unwrap_or_else(|e| panic!("{text}: {e:?}"));
        db.ioc_init().await;
        db.put_record_field_from_ca("R:E", "INP", EpicsValue::String(text.into()))
            .await
            .unwrap_or_else(|e| panic!("{text}: {e:?}"));
    }
}

/// A pva map with no string `pv` leaves the name empty (`pvxs/ioc/
/// pvalink_jlif.cpp:147-148` sets it only from a `pv` string; an integer `pv`
/// takes the unknown-integer branch, `:134-136`): softIocPVX takes
/// `{pva:{}}`, `{pva:{Q:2}}` and `{pva:{pv:3}}` at load and at a put.
#[epics_base_rs::epics_test]
async fn a_pva_map_without_pv_loads_and_puts() {
    for text in ["{pva:{}}", "{pva:{Q:2}}", "{pva:{pv:3}}"] {
        let db = format!(r#"record(ai, "R:E") {{ field(INP, {text}) }}"#);
        let (db, _) = IocBuilder::new()
            .register_link_type("pva", |_db| async { Vec::new() })
            .db_string(&db, &HashMap::new())
            .unwrap_or_else(|e| panic!("{text}: {e:?}"))
            .build()
            .await
            .unwrap_or_else(|e| panic!("{text}: {e:?}"));
        db.ioc_init().await;
        db.put_record_field_from_ca("R:E", "INP", EpicsValue::String(text.into()))
            .await
            .unwrap_or_else(|e| panic!("{text}: {e:?}"));
    }
}

/// DEVIATION: a pva name holding the identity-key separator is refused at
/// the put and the field keeps its old link.
#[epics_base_rs::epics_test]
async fn a_runtime_put_of_a_separator_pva_name_keeps_the_old_link() {
    let (db, _) = IocBuilder::new()
        .register_link_type("pva", |_db| async { Vec::new() })
        .db_string(
            r#"record(ai, "R:OK") { field(INP, "R:OK2") }
record(ai, "R:OK2") {}"#,
            &HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap();
    db.ioc_init().await;
    for text in ["{pva:\"A\u{1f}B\"}"] {
        let put = db
            .put_record_field_from_ca("R:OK", "INP", EpicsValue::String(text.into()))
            .await;
        assert!(put.is_err(), "{text}: {put:?}");
        let inp = db.get_pv("R:OK.INP").unwrap();
        assert_eq!(inp, EpicsValue::String("R:OK2 NPP NMS".into()), "{text}");
    }
}
