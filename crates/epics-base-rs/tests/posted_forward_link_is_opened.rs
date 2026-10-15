//! A record's own post of a link field installs a link C could only have
//! through `dbPutFieldLink` (`dbPut` refuses a link field, `dbAccess.c:1340`),
//! whose `dbAddLink` opens it (`dbAccess.c:1208`). So the post opens it once:
//! a forward link the lset serves gets its `scanForward` handle, one it opens
//! with no lset gets none.

use std::collections::HashMap;
use std::sync::Arc;

use epics_base_rs::server::database::{
    LinkForward, LinkForwardRefused, LinkHolder, LinkOpen, LinkPutOp, LinkSet, PvDatabase,
};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

#[derive(Default)]
struct Opens {
    opened: parking_lot::Mutex<Vec<String>>,
    forwards: parking_lot::Mutex<Vec<String>>,
}

#[async_trait::async_trait]
impl LinkSet for Opens {
    fn open_link(&self, _: LinkHolder<'_>, name: &[u8]) -> LinkOpen {
        self.opened
            .lock()
            .push(String::from_utf8_lossy(name).into_owned());
        if name.starts_with(b"PF:NOPE") {
            LinkOpen::NoLset
        } else {
            LinkOpen::Lset
        }
    }
    fn forward_handle(&self, name: &[u8]) -> Box<dyn LinkForward> {
        self.forwards
            .lock()
            .push(String::from_utf8_lossy(name).into_owned());
        Box::new(Down)
    }
    fn is_connected(&self, _: &[u8]) -> bool {
        false
    }
    fn get_cached_value(&self, _: &[u8]) -> Option<EpicsValue> {
        None
    }
    async fn get_value(&self, _: &[u8]) -> Option<EpicsValue> {
        None
    }
    async fn put_value(&self, _: &[u8], _: EpicsValue, _: LinkPutOp) -> Result<(), String> {
        Err("down".into())
    }
}

struct Down;

impl LinkForward for Down {
    fn scan_forward(&self) -> Result<(), LinkForwardRefused> {
        Err(LinkForwardRefused)
    }
}

const DB: &str = r#"record(fanout, "PF:FAN") {}"#;

async fn ioc(lset: Arc<Opens>) -> Arc<PvDatabase> {
    let db = IocBuilder::new()
        .register_link_type("pva", move |db| {
            let lset = lset.clone();
            async move {
                db.register_link_set("pva", lset).await;
                Vec::new()
            }
        })
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;
    db.ioc_init().await;
    db
}

#[epics_macros_rs::epics_test]
async fn a_posted_forward_link_is_opened_once_and_bound_only_with_an_lset() {
    let lset = Arc::new(Opens::default());
    let db = ioc(lset.clone()).await;
    assert!(lset.opened.lock().is_empty());
    db.post_fields(
        "PF:FAN",
        vec![
            ("LNK1".into(), EpicsValue::String(r#"{pva:"PF:U"}"#.into())),
            (
                "LNK2".into(),
                EpicsValue::String(r#"{pva:"PF:NOPE"}"#.into()),
            ),
        ],
    )
    .unwrap();
    let mut opened = lset.opened.lock().clone();
    opened.sort();
    assert_eq!(opened.len(), 2, "{opened:?}");
    assert!(opened[0].starts_with("PF:NOPE"), "{opened:?}");
    assert!(opened[1].starts_with("PF:U"), "{opened:?}");
    let forwards = lset.forwards.lock().clone();
    assert_eq!(forwards.len(), 1, "{forwards:?}");
    assert!(forwards[0].starts_with("PF:U"), "{forwards:?}");
}
