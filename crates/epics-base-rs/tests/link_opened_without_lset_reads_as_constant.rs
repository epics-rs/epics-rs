//! A link whose lset's `openLink` leaves it with no lset is C's
//! `plink->lset == NULL` on every path: `dbLinkIsConstant` (`dbLink.c:224`),
//! `dbGetLink`'s silent -1 (`:336`), `dbPutLink`'s `S_db_noLSET` (`:440`),
//! `dbScanFwdLink`'s no-op (`:479`). softIocPVX, `local:true` to a PV the IOC
//! does not have (`pvalink_lset.cpp:70-75`), each record processed once:
//!
//! ```text
//! NL:AI    SEVR NO_ALARM  STAT NO_ALARM  AMSG ""  UDF 0  VAL 0
//! NL:CALC  SEVR INVALID   STAT UDF       AMSG ""  UDF 1
//! NL:AO    SEVR NO_ALARM  STAT NO_ALARM  AMSG ""  VAL 3
//! NL:FL    SEVR INVALID   STAT UDF       AMSG ""  UDF 1
//! NL:AI.INP  "{pva:{pv:\"NL:NOPE\",local:true}}"
//! ```

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use epics_base_rs::server::database::{
    LinkForward, LinkForwardRefused, LinkOpen, LinkPutOp, LinkSet, PvDatabase,
};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

/// Leaves `NL:NOPE` with no lset at open, and counts every other call that
/// would mean the link still has one.
#[derive(Default)]
struct LocalOnly {
    calls: AtomicUsize,
}

#[async_trait::async_trait]
impl LinkSet for LocalOnly {
    fn open_link(&self, name: &[u8]) -> LinkOpen {
        if name.starts_with(b"NL:NOPE") {
            LinkOpen::NoLset
        } else {
            LinkOpen::Lset
        }
    }
    fn is_connected(&self, _: &[u8]) -> bool {
        self.calls.fetch_add(1, Ordering::Relaxed);
        false
    }
    fn get_cached_value(&self, _: &[u8]) -> Option<EpicsValue> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        None
    }
    async fn get_value(&self, _: &[u8]) -> Option<EpicsValue> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        None
    }
    async fn put_value(&self, _: &[u8], _: EpicsValue, _: LinkPutOp) -> Result<(), String> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        Err("no lset".into())
    }
    fn forward_handle(&self, _: &[u8]) -> Box<dyn LinkForward> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        Box::new(Refused)
    }
}

struct Refused;

impl LinkForward for Refused {
    fn scan_forward(&self) -> Result<(), LinkForwardRefused> {
        Err(LinkForwardRefused)
    }
}

const DB: &str = r#"
record(ai, "NL:AI") { field(INP, {pva:{pv:"NL:NOPE", local:true}}) }
record(calc, "NL:CALC") { field(INPA, {pva:{pv:"NL:NOPE", local:true}}) field(CALC, "A+1") }
record(ao, "NL:AO") { field(OUT, {pva:{pv:"NL:NOPE", local:true}}) field(VAL, "3") }
record(longout, "NL:FL") { field(FLNK, {pva:{pv:"NL:NOPE", local:true}}) }
"#;

fn field(db: &PvDatabase, name: &str) -> String {
    db.get_pv(name).unwrap().to_string()
}

#[epics_macros_rs::epics_test]
async fn a_link_opened_without_an_lset_reads_puts_and_forwards_as_constant() {
    let lset = Arc::new(LocalOnly::default());
    let installed = lset.clone();
    let db = IocBuilder::new()
        .register_link_type("pva", move |db| {
            let lset = installed.clone();
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
    for rec in ["NL:AI", "NL:CALC", "NL:AO", "NL:FL"] {
        db.process_record(rec).await.unwrap();
    }
    // SEVR 3 INVALID, STAT 17 UDF_ALARM.
    let want = [
        ("NL:AI.SEVR", "0"),
        ("NL:AI.STAT", "0"),
        ("NL:AI.AMSG", ""),
        ("NL:AI.UDF", "0"),
        ("NL:AI.VAL", "0"),
        ("NL:CALC.SEVR", "3"),
        ("NL:CALC.STAT", "17"),
        ("NL:CALC.AMSG", ""),
        ("NL:CALC.UDF", "1"),
        ("NL:AO.SEVR", "0"),
        ("NL:AO.STAT", "0"),
        ("NL:AO.AMSG", ""),
        ("NL:AO.VAL", "3"),
        ("NL:FL.SEVR", "3"),
        ("NL:FL.STAT", "17"),
        ("NL:FL.AMSG", ""),
        ("NL:FL.UDF", "1"),
        ("NL:AI.INP", r#"{pva:{pv:"NL:NOPE",local:true}}"#),
    ];
    for (name, want) in want {
        assert_eq!(field(&db, name), want, "{name}");
    }
    assert_eq!(
        lset.calls.load(Ordering::Relaxed),
        0,
        "the lset was reached"
    );
}
