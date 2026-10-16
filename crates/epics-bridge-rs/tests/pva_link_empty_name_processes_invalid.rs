//! An empty pva name loads, puts and processes with no channel: pvxs
//! `pvaOpenLink` opens nothing for it (`ioc/pvalink_lset.cpp:97-98`). A null,
//! bool or integer root names no channel either (`pvalink_jlif.cpp:69-139`),
//! nor a map with no string `pv` member (`:147-148`).
//! DEVIATION: softIocPVX then segfaults at the record's first process, in
//! `pvaDoLocked` locking the never-opened channel (`pvalink_lset.cpp:702`);
//! the port reads the link as unconnected, LINK/INVALID.
//!
//! Tokio only: the pvalink runtime is.
#![cfg(all(feature = "pvalink", tokio_backend))]

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use epics_base_rs::server::database::{ProcStack, PvDatabase};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;
use epics_bridge_rs::pvalink::install_pvalink_resolver;

const DB: &str = r#"
record(ai, "E:L") { field(INP, {pva:""}) }
record(ai, "E:M") { field(INP, {pva:{pv:""}}) }
record(ai, "E:G") { field(INP, {pva:5}) }
record(ai, "E:H") { field(INP, {pva:true}) }
record(ai, "E:I") { field(INP, {pva:null}) }
record(ai, "E:K") { field(INP, {pva:{}}) }
record(ai, "E:U") { field(INP, {pva:{Q:2}}) }
record(ai, "E:S") { field(INP, {pva:{pv:3}}) }
record(ai, "E:P") { field(INP, "E:OK") }
record(ai, "E:PG") { field(INP, "E:OK") }
record(ai, "E:PH") { field(INP, "E:OK") }
record(ai, "E:PI") { field(INP, "E:OK") }
record(ai, "E:PK") { field(INP, "E:OK") }
record(ai, "E:PU") { field(INP, "E:OK") }
record(ai, "E:PS") { field(INP, "E:OK") }
record(ai, "E:OK") {}
"#;

/// Each runtime put: the record and the text softIocPVX takes into its INP.
const PUTS: &[(&str, &str)] = &[
    ("E:P", r#"{pva:{pv:""}}"#),
    ("E:PG", "{pva:5}"),
    ("E:PH", "{pva:true}"),
    ("E:PI", "{pva:null}"),
    ("E:PK", "{pva:{}}"),
    ("E:PU", "{pva:{Q:2}}"),
    ("E:PS", "{pva:{pv:3}}"),
];

/// The records that hold a link with no channel name from load.
const LOADED: &[&str] = &["E:L", "E:M", "E:G", "E:H", "E:I", "E:K", "E:U", "E:S"];

#[tokio::test(flavor = "multi_thread")]
async fn empty_pva_name_processes_link_invalid_and_opens_nothing() {
    let db: Arc<PvDatabase> = IocBuilder::new()
        .register_link_type("pva", |_db| async { Vec::new() })
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;
    let resolver = install_pvalink_resolver(&db).await;
    for (rec, text) in PUTS {
        db.put_record_field_from_ca(rec, "INP", EpicsValue::String((*text).into()))
            .await
            .unwrap_or_else(|e| panic!("softIocPVX takes {text}: {e:?}"));
    }

    let records: Vec<&str> = LOADED
        .iter()
        .copied()
        .chain(PUTS.iter().map(|(rec, _)| *rec))
        .collect();
    for _ in 0..3 {
        for rec in &records {
            db.process_record_with_links(rec, &mut ProcStack::new())
                .await
                .or_else(epics_base_rs::error::CaError::unless_process_status)
                .unwrap_or_else(|e| panic!("{rec}: {e:?}"));
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    for rec in &records {
        let alarm = (
            db.get_pv(format!("{rec}.STAT")).unwrap(),
            db.get_pv(format!("{rec}.SEVR")).unwrap(),
        );
        assert_eq!(
            format!("{alarm:?}"),
            format!("{:?}", (EpicsValue::Short(14), EpicsValue::Short(3))),
            "{rec}"
        );
    }
    let channels = resolver.channel_diagnostics();
    assert!(channels.is_empty(), "{channels:?}");
}
