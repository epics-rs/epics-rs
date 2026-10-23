//! C calls `lset->openLink` at two places: `doResolveLinks` at iocInit
//! (`iocInit.c:576-577` → `dbInitLink` → `dbJLinkInit`, `dbJLink.c:459`) and
//! `dbPutFieldLink`'s `dbAddLink` (`dbAccess.c:1208`), under the put's
//! `dbScanLockMany`. So each link is opened once at init, and each put of a
//! link opens it once more before `dbPutField` returns. A bare CA name is a
//! `PV_LINK`, which `dbCaAddLink` takes with no `openLink` (`dbLink.c:128`).

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use epics_base_rs::server::database::{
    LinkHolder, LinkOpen, LinkPutOp, LinkRef, LinkSet, PvDatabase,
};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

/// Counts every `open_link`, and every other call that reaches it.
#[derive(Default)]
struct Opens {
    opened: parking_lot::Mutex<Vec<String>>,
    calls: AtomicUsize,
}

#[async_trait::async_trait]
impl LinkSet for Opens {
    fn open_link(&self, _: LinkHolder<'_>, link: LinkRef<'_>) -> LinkOpen {
        let name = link.name;
        self.opened
            .lock()
            .push(String::from_utf8_lossy(name).into_owned());
        if name.starts_with(b"OL:NOPE") {
            LinkOpen::NoLset
        } else {
            LinkOpen::Lset
        }
    }
    fn is_connected(&self, _: LinkRef<'_>) -> bool {
        self.calls.fetch_add(1, Ordering::Relaxed);
        false
    }
    fn get_cached_value(&self, _: LinkRef<'_>) -> Option<EpicsValue> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        None
    }
    async fn get_value(&self, _: LinkRef<'_>) -> Option<EpicsValue> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        None
    }
    async fn put_value(&self, _: LinkRef<'_>, _: EpicsValue, _: LinkPutOp) -> Result<(), String> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        Err("down".into())
    }
}

const DB: &str = r#"
record(ai, "OL:AI") { field(INP, {pva:"OL:X"}) }
record(ai, "OL:URL") { field(INP, "pva://OL:Y") }
record(calc, "OL:CALC") { field(INPA, {pva:"OL:Z"}) field(INPB, {pva:"OL:W"}) }
record(ao, "OL:AO") { field(OUT, {pva:"OL:V"}) }
record(longout, "OL:FL") { field(FLNK, {pva:"OL:U"}) }
record(ai, "OL:CA") { field(INP, "OL:REMOTE CA") }
"#;

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
    // A second barrier crossing opens nothing again.
    db.ioc_init().await;
    db
}

fn opened(lset: &Opens) -> Vec<String> {
    let mut names = lset.opened.lock().clone();
    names.sort();
    names
}

#[epics_macros_rs::epics_test]
async fn each_link_is_opened_once_at_init() {
    let lset = Arc::new(Opens::default());
    let _db = ioc(lset.clone()).await;
    assert_eq!(
        opened(&lset),
        ["OL:U", "OL:V", "OL:W", "OL:X", "OL:Y", "OL:Z"]
    );
}

#[epics_macros_rs::epics_test]
async fn a_link_put_opens_the_link_before_the_put_returns() {
    let lset = Arc::new(Opens::default());
    let db = ioc(lset.clone()).await;
    let init = lset.opened.lock().len();
    db.put_record_field_from_ca_no_notify(
        "OL:AI",
        "INP",
        EpicsValue::String(r#"{pva:"OL:Q"}"#.into()),
    )
    .await
    .unwrap();
    assert_eq!(lset.opened.lock()[init..], ["OL:Q"]);
    // Not a link's text: nothing opens. The put processes OL:AI, whose dead
    // INP is the put's status.
    db.put_record_field_from_ca_no_notify("OL:AI", "VAL", EpicsValue::Double(1.0))
        .await
        .or_else(epics_base_rs::error::CaError::unless_process_status)
        .unwrap();
    assert_eq!(lset.opened.lock().len(), init + 1);
}

/// The put's open decides the link before the record's next cycle: one the
/// lset leaves with no lset is `plink->lset == NULL` from that put on.
#[epics_macros_rs::epics_test]
async fn a_put_opened_without_an_lset_reads_as_constant() {
    let lset = Arc::new(Opens::default());
    let db = ioc(lset.clone()).await;
    db.put_record_field_from_ca_no_notify(
        "OL:AI",
        "INP",
        EpicsValue::String(r#"{pva:{pv:"OL:NOPE", local:true}}"#.into()),
    )
    .await
    .unwrap();
    lset.calls.store(0, Ordering::Relaxed);
    db.process_record("OL:AI").await.unwrap();
    assert_eq!(db.get_pv("OL:AI.SEVR").unwrap().text_g(), "0");
    assert_eq!(
        lset.calls.load(Ordering::Relaxed),
        0,
        "the lset was reached"
    );
}

/// FLNK is no device link: once a put left it with no lset, the next put
/// fails `dbLinkIsDefined` with `S_db_badHWaddr` before `dbAddLink`
/// (`dbAccess.c:1168-1174`), clearing it included, and the field keeps its
/// text. FLNK lives in the `dbCommon` cache, not a link slot.
#[epics_macros_rs::epics_test]
async fn a_forward_link_left_with_no_lset_refuses_the_next_put() {
    let lset = Arc::new(Opens::default());
    let db = ioc(lset.clone()).await;
    let nope = r#"{pva:{pv:"OL:NOPE", local:true}}"#;
    db.put_record_field_from_ca_no_notify("OL:FL", "FLNK", EpicsValue::String(nope.into()))
        .await
        .unwrap();
    let opens = lset.opened.lock().len();
    for text in [r#"{pva:"OL:Q"}"#, ""] {
        let put = db
            .put_record_field_from_ca_no_notify("OL:FL", "FLNK", EpicsValue::String(text.into()))
            .await;
        assert!(put.is_err(), "{text:?}: {put:?}");
        assert_eq!(
            db.get_pv("OL:FL.FLNK").unwrap(),
            EpicsValue::String(nope.into()),
            "{text:?}"
        );
    }
    assert_eq!(lset.opened.lock().len(), opens, "a refused put opened");
}
