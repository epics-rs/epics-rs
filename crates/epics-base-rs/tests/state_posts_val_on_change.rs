//! `state` posts VAL only when `strncmp(oval, val)` differs and clears UDF on
//! every process (`stateRecord.c:95-120`). softIoc R7.0.10.1, `camonitor
//! ST.VAL` while `dbpf` puts run, run, PROC 1, stop, "" and a 26-character
//! string: it shows idle (the connect), run, stop, "" and the first 19
//! characters, and `dbgf` reads OVAL = VAL and UDF 0.

use std::collections::HashMap;

use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::types::{DbFieldType, EpicsValue};

#[epics_macros_rs::epics_test]
async fn state_posts_val_only_on_a_change_staterecord_c_115() {
    let (db, _h) = IocBuilder::new()
        .db_string(
            r#"record(state, "ST") { field(VAL, "idle") }"#,
            &HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap();
    let mut rx = db
        .get_record("ST")
        .unwrap()
        .write()
        .add_subscriber(
            "VAL",
            1,
            DbFieldType::String,
            (EventMask::VALUE | EventMask::LOG).bits(),
        )
        .unwrap();

    let mut posts = Vec::new();
    for (field, value) in [
        ("VAL", "run"),
        ("VAL", "run"),
        ("PROC", "1"),
        ("VAL", "stop"),
        ("VAL", ""),
        ("VAL", "abcdefghijklmnopqrstuvwxyz"),
    ] {
        let value = match field {
            "PROC" => EpicsValue::Char(1),
            _ => EpicsValue::String(value.into()),
        };
        db.put_record_field_from_ca("ST", field, value)
            .await
            .unwrap();
        while let Ok(event) = rx.try_recv() {
            posts.push(event.snapshot.value.to_string());
        }
    }
    assert_eq!(posts, ["run", "stop", "", "abcdefghijklmnopqrs"]);

    let get = |f: &str| db.get_pv(&format!("ST.{f}")).ok();
    assert_eq!(
        get("OVAL"),
        Some(EpicsValue::String("abcdefghijklmnopqrs".into()))
    );
    assert_eq!(get("UDF").and_then(|v| v.to_f64()), Some(0.0));
}

/// The UDF INVALID -> NO_ALARM clear on the first PROC changes no string, so
/// C posts no VAL even to a DBE_ALARM subscriber: `monitor()` hands
/// `recGblResetAlarms`' mask to `db_post_events` only inside the `strncmp`
/// branch (`stateRecord.c:118-121`). softIoc R7.0.10.1, `camonitor -m va ST`
/// over PROC, PROC, `dbpf ST.VAL x`: the connect, then `x` alone.
#[epics_macros_rs::epics_test]
async fn an_alarm_only_change_posts_no_val_staterecord_c_118() {
    let (db, _h) = IocBuilder::new()
        .db_string(r#"record(state, "ST") { }"#, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    let mut rx = db
        .get_record("ST")
        .unwrap()
        .write()
        .add_subscriber(
            "VAL",
            1,
            DbFieldType::String,
            (EventMask::VALUE | EventMask::ALARM).bits(),
        )
        .unwrap();

    let mut posts = Vec::new();
    for (field, value) in [
        ("PROC", EpicsValue::Char(1)),
        ("PROC", EpicsValue::Char(1)),
        ("VAL", EpicsValue::String("x".into())),
    ] {
        db.put_record_field_from_ca("ST", field, value)
            .await
            .unwrap();
        while let Ok(event) = rx.try_recv() {
            posts.push(event.snapshot.value.to_string());
        }
    }
    assert_eq!(posts, ["x"]);
    let get = |f: &str| db.get_pv(&format!("ST.{f}")).ok().and_then(|v| v.to_f64());
    assert_eq!((get("UDF"), get("SEVR")), (Some(0.0), Some(0.0)));
}

/// permissive's `monitor()` has the same shape (`permissiveRecord.c:107-110`).
#[epics_macros_rs::epics_test]
async fn an_alarm_only_change_posts_no_val_permissiverecord_c_107() {
    let (db, _h) = IocBuilder::new()
        .db_string(r#"record(permissive, "PM") { }"#, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    let mut rx = db
        .get_record("PM")
        .unwrap()
        .write()
        .add_subscriber(
            "VAL",
            1,
            DbFieldType::Short,
            (EventMask::VALUE | EventMask::ALARM).bits(),
        )
        .unwrap();

    let mut posts = Vec::new();
    for (field, value) in [
        ("PROC", EpicsValue::Char(1)),
        ("PROC", EpicsValue::Char(1)),
        ("VAL", EpicsValue::Short(1)),
    ] {
        db.put_record_field_from_ca("PM", field, value)
            .await
            .unwrap();
        while let Ok(event) = rx.try_recv() {
            posts.push(event.snapshot.value.to_string());
        }
    }
    assert_eq!(posts, ["1"]);
    let get = |f: &str| db.get_pv(&format!("PM.{f}")).ok().and_then(|v| v.to_f64());
    assert_eq!((get("UDF"), get("SEVR")), (Some(0.0), Some(0.0)));
}
