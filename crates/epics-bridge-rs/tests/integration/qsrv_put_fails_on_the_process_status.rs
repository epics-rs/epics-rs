//! A QSRV put whose processing returned non-zero fails with that status's
//! `errSymLookup` text; a blocking put does not look at it.
//!
//! pvxs runs `dbProcess` itself after a non-blocking put and throws on any
//! non-zero status (`doPostProcessing`, `iocsource.cpp:419-423`), while
//! `record[block=true]` goes through `dbProcessNotify`, which never reads it.
//! Measured on softIocPVX with `record(ao,"F:ao"){field(OUT,"NOPV:X CA")}`:
//! `pvxput F:ao.PROC 1` and `pvxput -r 'record[process=true]' F:ao.DESC x`
//! print "Error (65535,65535)", `pvxput -r 'record[block=true]' F:ao.PROC 1`
//! succeeds; a group put stops at the member that failed, atomic or not, with
//! that member's value written and the later members' not.
#![cfg(all(feature = "qsrv-core", tokio_backend))]

use std::sync::Arc;

use epics_base_rs::server::database::{LinkPutOp, LinkRef, LinkSet, PvDatabase};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::{DbFieldType, EpicsValue};
use epics_bridge_rs::error::BridgeError;
use epics_bridge_rs::qsrv::channel::BridgeChannel;
use epics_bridge_rs::qsrv::group::GroupChannel;
use epics_bridge_rs::qsrv::{BridgeProvider, NtType, ProcessMode, PutOptions};
use epics_pva_rs::pvdata::{PvField, PvStructure, ScalarValue};

/// Every remote channel down: `dbCa`'s -1 on a put (`dbCa.c:558-560`).
struct Down;

#[async_trait::async_trait]
impl LinkSet for Down {
    fn is_connected(&self, _: LinkRef<'_>) -> bool {
        false
    }
    fn get_cached_value(&self, _: LinkRef<'_>) -> Option<EpicsValue> {
        None
    }
    async fn get_value(&self, _: LinkRef<'_>) -> Option<EpicsValue> {
        None
    }
    async fn put_value(&self, _: LinkRef<'_>, _: EpicsValue, _: LinkPutOp) -> Result<(), String> {
        Err("channel disconnected".into())
    }
}

const DB: &str = r#"
record(ao, "F:ao") { field(OUT, "NOPV:X CA") }
record(longout, "S:lo") { field(OUT, "NOPV:X CA") }
record(ao, "G:good") {}
"#;

const GROUPS: &str = r#"{
    "G:atom": {
        "+atomic": true,
        "bad": { "+channel": "F:ao.VAL", "+type": "plain", "+putorder": 0 },
        "good": { "+channel": "G:good.VAL", "+type": "plain", "+putorder": 1 }
    },
    "G:na": {
        "+atomic": false,
        "bad": { "+channel": "F:ao.VAL", "+type": "plain", "+putorder": 0 },
        "good": { "+channel": "G:good.VAL", "+type": "plain", "+putorder": 1 }
    }
}"#;

const STATUS_TEXT: &str = "Error (65535,65535)";

async fn ioc() -> Arc<PvDatabase> {
    let db = Arc::new(PvDatabase::new());
    db.register_link_set("ca", Arc::new(Down)).await;
    let (db, _) = IocBuilder::new()
        .with_database(db)
        .db_string(DB, &Default::default())
        .unwrap()
        .build()
        .await
        .unwrap();
    db.ioc_init().await;
    db
}

fn channel(db: &Arc<PvDatabase>, record: &str, field: &str, dbf: DbFieldType) -> BridgeChannel {
    BridgeChannel::from_cached(
        db.clone(),
        format!("{record}.{field}").into(),
        record.into(),
        field.into(),
        NtType::Scalar,
        dbf,
    )
}

fn scalar(v: ScalarValue) -> PvStructure {
    let mut put = PvStructure::new("epics:nt/NTScalar:1.0");
    put.fields.push(("value".into(), PvField::Scalar(v)));
    put
}

fn opts(process: ProcessMode, block: bool) -> PutOptions {
    PutOptions { process, block }
}

fn is_status_text(r: &Result<(), BridgeError>) -> bool {
    matches!(r, Err(BridgeError::PutRejected(m)) if m == STATUS_TEXT)
}

#[tokio::test]
async fn a_put_that_processes_fails_on_the_status() {
    let db = ioc().await;
    let val = channel(&db, "F:ao", "VAL", DbFieldType::Double);
    let one = scalar(ScalarValue::Double(1.0));
    let passive = val
        .put_with_options(&one, opts(ProcessMode::Passive, false))
        .await;
    assert!(is_status_text(&passive), "passive put: {passive:?}");

    let desc = channel(&db, "F:ao", "DESC", DbFieldType::String);
    let forced = desc
        .put_with_options(
            &scalar(ScalarValue::String("x".into())),
            opts(ProcessMode::Force, false),
        )
        .await;
    assert!(is_status_text(&forced), "process=true put: {forced:?}");
}

#[tokio::test]
async fn a_put_that_does_not_read_the_status_succeeds() {
    let db = ioc().await;
    let val = channel(&db, "F:ao", "VAL", DbFieldType::Double);
    let one = scalar(ScalarValue::Double(1.0));
    for (mode, block) in [
        (ProcessMode::Passive, true),
        (ProcessMode::Force, true),
        (ProcessMode::Inhibit, false),
    ] {
        let put = val.put_with_options(&one, opts(mode, block)).await;
        assert!(put.is_ok(), "{mode:?} block={block}: {put:?}");
    }
    // longout's soft device drops dbPutLink's status (devLoSoft.c:45-46).
    let lo = channel(&db, "S:lo", "VAL", DbFieldType::Long);
    let put = lo
        .put_with_options(
            &scalar(ScalarValue::Int(1)),
            opts(ProcessMode::Passive, false),
        )
        .await;
    assert!(put.is_ok(), "longout: {put:?}");
}

#[tokio::test]
async fn a_group_put_stops_at_the_member_whose_processing_failed() {
    for group in ["G:atom", "G:na"] {
        let db = ioc().await;
        let provider = BridgeProvider::new(db.clone());
        provider.load_group_config(GROUPS).expect("load");
        let def = provider.groups().get(group).cloned().expect("group");
        let ch = GroupChannel::new(db.clone(), def);
        let mut put = PvStructure::new("structure");
        put.fields
            .push(("bad".into(), PvField::Scalar(ScalarValue::Double(5.0))));
        put.fields
            .push(("good".into(), PvField::Scalar(ScalarValue::Double(7.0))));
        let r = ch
            .put_with_options(
                &put,
                opts(ProcessMode::Passive, false),
                None,
                &Default::default(),
            )
            .await;
        assert!(is_status_text(&r), "{group}: {r:?}");
        let val = |name: &str| db.get_record(name).unwrap().read().record.get_field("VAL");
        assert_eq!(val("F:ao"), Some(EpicsValue::Double(5.0)), "{group}");
        assert_eq!(val("G:good"), Some(EpicsValue::Double(0.0)), "{group}");
    }
}
