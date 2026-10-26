//! A PV Access link is C's `JSON_LINK` (`dbStaticLib.c:2280-2287`), so every
//! `plink->type` test a record or its device support makes sends it where a
//! `JSON_LINK` goes: never the `CA_LINK` put-callback, and in a type switch
//! the arm that keeps the switch's initializer. No C IOC with both the calc
//! module and pvxs links is available locally, so the pva expectations are
//! read from the cited C lines; the `{state:}` one is measured on oracleIoc.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use epics_base_rs::server::database::filters::sync::db_state_registry;
use epics_base_rs::server::database::{
    DynLinkSet, LinkDbfType, LinkMetadata, LinkPutOp, LinkRef, LinkSet, ProcStack, PvDatabase,
};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::records::acalcout::AcalcoutRecord;
use epics_base_rs::server::records::scalcout::ScalcoutRecord;
use epics_base_rs::server::records::sseq::SseqRecord;
use epics_base_rs::types::EpicsValue;

type Puts = Arc<Mutex<Vec<(EpicsValue, LinkPutOp)>>>;

/// A connected channel of type `dbf` and `elements` elements that records
/// each put.
struct Lset {
    dbf: LinkDbfType,
    elements: i64,
    puts: Puts,
}

#[async_trait::async_trait]
impl LinkSet for Lset {
    fn is_connected(&self, _: LinkRef<'_>) -> bool {
        true
    }
    fn get_cached_value(&self, _: LinkRef<'_>) -> Option<EpicsValue> {
        Some(EpicsValue::Double(0.0))
    }
    async fn get_value(&self, _: LinkRef<'_>) -> Option<EpicsValue> {
        Some(EpicsValue::Double(0.0))
    }
    fn link_metadata(&self, _: LinkRef<'_>) -> Option<LinkMetadata> {
        Some(LinkMetadata {
            dbf_type: Some(self.dbf),
            element_count: Some(self.elements),
            ..LinkMetadata::default()
        })
    }
    async fn put_value(
        &self,
        _: LinkRef<'_>,
        value: EpicsValue,
        op: LinkPutOp,
    ) -> Result<(), String> {
        self.puts.lock().unwrap().push((value, op));
        Ok(())
    }
}

const DB: &str = r#"
record(sseq, "S") { field(DO1, "3") field(LNK1, "{pva:\"T\"}") field(WAIT1, "Wait") }
record(scalcout, "W") { field(CALC, "7") field(OUT, "{pva:\"T\"}") field(WAIT, "Wait") }
record(scalcout, "C") { field(CALC, "7") field(OUT, "{pva:\"T\"}") }
record(scalcout, "SS") { field(CALC, "'abc'") field(OUT, "{state:\"SA\"}") }
record(acalcout, "A") { field(NELM, "4") field(CALC, "7") field(OUT, "{pva:\"T\"}") }
"#;

async fn build(dbf: LinkDbfType, elements: i64) -> (Arc<PvDatabase>, Puts) {
    let puts = Puts::default();
    let lset: DynLinkSet = Arc::new(Lset {
        dbf,
        elements,
        puts: Arc::clone(&puts),
    });
    let ioc = IocBuilder::new()
        .register_record_type("sseq", || Box::new(SseqRecord::default()))
        .register_record_type("scalcout", || Box::new(ScalcoutRecord::default()))
        .register_record_type("acalcout", || Box::new(AcalcoutRecord::default()))
        .register_link_type("pva", move |db| async move {
            db.register_link_set("pva", lset).await;
            Vec::new()
        })
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build_ioc()
        .await
        .unwrap();
    let db = ioc.database().clone();
    assert!(ioc.run().await);
    (db, puts)
}

fn read(db: &PvDatabase, pv: &str) -> i16 {
    db.get_pv(pv)
        .ok()
        .and_then(|v| v.to_f64())
        .map(|f| f as i16)
        .unwrap_or_else(|| panic!("{pv} not readable as a number"))
}

/// Processes `rec` and returns the one put its link made.
async fn the_put(db: &PvDatabase, puts: &Puts, rec: &str) -> (EpicsValue, LinkPutOp) {
    db.process_record_with_links(rec, &mut ProcStack::new())
        .await
        .unwrap();
    for _ in 0..400 {
        if let Some(put) = puts.lock().unwrap().first() {
            return put.clone();
        }
        epics_base_rs::runtime::task::sleep(Duration::from_millis(5)).await;
    }
    panic!("{rec} never put to its link");
}

/// sseq `processCallback` (sseqRecord.c:717, :739, :763) asks for the
/// put-callback only of a `CA_LINK`; `checkLinks` raises `WERR1` only for a
/// `DB_LINK` and rescinds it for any other type (:912-926).
#[epics_base_rs::epics_test]
async fn sseq_does_not_wait_on_a_pva_link() {
    let (db, puts) = build(LinkDbfType::Double, 1).await;
    assert_eq!(the_put(&db, &puts, "S").await.1, LinkPutOp::Plain);
    assert_eq!(read(&db, "S.WTG1"), 0);
    assert_eq!(read(&db, "S.WERR1"), 0);
}

/// `devsCalcoutSoft.c:78`: `plink->type == CA_LINK && wait` is the only
/// branch that puts with a callback and sets PACT.
#[epics_base_rs::epics_test]
async fn scalcout_does_not_wait_on_a_pva_link() {
    let (db, puts) = build(LinkDbfType::Double, 1).await;
    assert_eq!(the_put(&db, &puts, "W").await.1, LinkPutOp::Plain);
    assert_eq!(read(&db, "W.PACT"), 0);
}

/// `devsCalcoutSoft.c:113-125`: only the `CA_LINK` and `DB_LINK` arms read
/// the target's type, so a `JSON_LINK` keeps `field_type = 0`, which is
/// `DBF_STRING`, and puts `DBR_STRING` from OSV (:131-134) even to a numeric
/// target.
#[epics_base_rs::epics_test]
async fn scalcout_puts_osv_to_a_pva_target() {
    let (db, puts) = build(LinkDbfType::Double, 1).await;
    let put = the_put(&db, &puts, "C").await;
    let osv = db.get_pv("C.OSV").unwrap();
    assert!(matches!(&osv, EpicsValue::String(s) if !s.is_empty()));
    assert_eq!(put, (osv, LinkPutOp::Plain));
}

/// The same `field_type = 0` arm for a `{state:}` link. Measured on
/// oracleIoc: CALC `'abc'` with OUT `{state:"SA"}` leaves OSV "abc" and SA
/// TRUE, the state link's string put.
#[epics_base_rs::epics_test]
async fn scalcout_puts_osv_to_a_state_link() {
    let (db, _) = build(LinkDbfType::Double, 1).await;
    db.process_record_with_links("SS", &mut ProcStack::new())
        .await
        .unwrap();
    assert_eq!(
        db.get_pv("SS.OSV").unwrap(),
        EpicsValue::String("abc".into())
    );
    assert!(db_state_registry().get("SA"));
}

/// `devaCalcoutSoft.c:75-80` sizes only a `CA_LINK` or a `DB_LINK`; a
/// `JSON_LINK` falls to `dbNameToAddr` of its NULL `pv_link.pvname` and puts
/// the scalar VAL. That C defect is not copied: a pva array target takes the
/// array.
#[epics_base_rs::epics_test]
async fn acalcout_puts_its_array_to_a_pva_array_target() {
    let (db, puts) = build(LinkDbfType::Double, 4).await;
    assert_eq!(
        the_put(&db, &puts, "A").await,
        (
            EpicsValue::DoubleArray(vec![7.0; 4].into()),
            LinkPutOp::Plain
        )
    );
}
