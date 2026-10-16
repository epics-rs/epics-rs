//! A link with no scheme that is not a DB link is a CA link (`dbLink.c:118-130`):
//! its value, alarm and time come from the `ca` link set alone, never from
//! another installed lset. Two sets registered once raced in map order, and a
//! CP holder read the `pva` set's later monitor: one event behind its source.

mod calink_ioc;

use std::collections::HashMap;
use std::sync::Arc;

use epics_base_rs::server::database::{LinkSet, PvDatabase};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

const DECOY: f64 = -1.0;

/// A `pva` set that claims every name, connected, MAJOR, with a value.
struct Decoy;

#[async_trait::async_trait]
impl LinkSet for Decoy {
    fn is_connected(&self, _: &[u8]) -> bool {
        true
    }
    fn get_cached_value(&self, _: &[u8]) -> Option<EpicsValue> {
        Some(EpicsValue::Double(DECOY))
    }
    async fn get_value(&self, _: &[u8]) -> Option<EpicsValue> {
        Some(EpicsValue::Double(DECOY))
    }
    fn alarm_severity(&self, _: &[u8]) -> Option<i32> {
        Some(2)
    }
}

async fn build(db_text: &str) -> Arc<PvDatabase> {
    let db = IocBuilder::new()
        .db_string(db_text, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;
    db.register_link_set("pva", Arc::new(Decoy)).await;
    db.initialize_link_locality().await;
    calink_ioc::connect(&db).await;
    db
}

fn num(db: &PvDatabase, pv: &str) -> f64 {
    db.get_pv(pv).unwrap().to_f64().unwrap()
}

/// A name nothing serves: the CA link is down, so CA, CP and CPP inputs all
/// read nothing and raise LINK/INVALID (`dbCa.c:430-435`), whatever another
/// set would answer.
#[epics_macros_rs::epics_test]
async fn an_unserved_name_reads_as_a_down_ca_link() {
    let db = build(
        r#"record(calc, "N") {
            field(INPA, "NOWHERE:A CA") field(INPB, "NOWHERE:B CP")
            field(INPC, "NOWHERE:C CPP") field(CALC, "A+B+C")
        }"#,
    )
    .await;
    db.process_record("N").await.unwrap();
    for f in ["N.A", "N.B", "N.C"] {
        assert_eq!(num(&db, f), 0.0, "{f} was read from the pva set");
    }
    assert_eq!(db.get_pv("N.SEVR").unwrap().to_f64(), Some(3.0));
    assert_eq!(
        db.get_pv("N.STAT").unwrap().to_f64(),
        Some(14.0),
        "LINK, not the pva set's alarm"
    );
}

/// A local source with a second set installed: the connect-time event and
/// each later one reach CP and CPP holders, and a CA input read, at the
/// source's value. One database per map seed, so either iteration order of
/// the old race is met.
#[epics_macros_rs::epics_test]
async fn a_local_source_is_read_through_ca_on_every_event() {
    for _ in 0..8 {
        let db = build(
            r#"record(ao, "X") { field(VAL, "0.5") }
            record(calc, "CP") { field(INPA, "X CP") field(CALC, "A") }
            record(calc, "CPP") { field(INPA, "X CPP") field(CALC, "A") }
            record(calc, "CA") { field(INPA, "X CA") field(CALC, "A") }"#,
        )
        .await;
        calink_ioc::until("the connect-time event", || {
            num(&db, "CP") == 0.5 && num(&db, "CPP") == 0.5
        })
        .await;
        for v in [1.0, 2.0, 3.0] {
            db.put_pv_no_process("X", EpicsValue::Double(v))
                .await
                .unwrap();
            db.process_record("X").await.unwrap();
            calink_ioc::until(&format!("CP and CPP follow X = {v}"), || {
                num(&db, "CP") == v && num(&db, "CPP") == v
            })
            .await;
            db.process_record("CA").await.unwrap();
            assert_eq!(num(&db, "CA"), v);
            assert_eq!(db.get_pv("CA.SEVR").unwrap().to_f64(), Some(0.0));
        }
    }
}
